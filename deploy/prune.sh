#!/usr/bin/env bash
# llmux2-prune.service: regenerate the gateway config without swept dead
# routes and deploy it through the normal slot-swap path.
#
# The site's importer arguments live in ~/llmux2/import.flags, one argument
# per line (comments allowed) — the same supplemental flags the original
# import used (e.g. --omniroute-fallback, --nous-state, --kilo-free-models),
# so a prune regenerates the same catalog, not a smaller one.
set -euo pipefail

dir="$HOME/llmux2"
flags=()
if [[ -f "$dir/import.flags" ]]; then
    while IFS= read -r line; do
        [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]] && continue
        flags+=("$line")
    done < "$dir/import.flags"
fi

# The deploy gate rejects a config whose listen differs from the installed
# one; read the deployed address back from active.json instead of hardcoding.
listen="$(grep -o '"listen": *"[^"]*"' "$dir/active.json" | head -1 | sed 's/.*"\([^"]*\)"$/\1/')"
[[ -n "$listen" ]] || { echo "llmux2-prune: no deployed listen in $dir/active.json" >&2; exit 1; }

[[ -f "$dir/dead-models.json" ]] || { echo "llmux2-prune: no sweep output yet; nothing to do"; exit 0; }

# Only the candidate config is removed: deploy atomically copies it to the
# idle slot, and the shared secrets directory must stay live — the importer
# never overwrites existing secret files, so reuse is safe.
rm -f "$dir/llmux-next.toml"
"$dir/bin/llmux-import" \
    --input "$dir/export.json" \
    --output "$dir/llmux-next.toml" \
    --secrets-dir "$dir/secrets" \
    --exclude-models "$dir/dead-models.json" \
    --listen "$listen" \
    "${flags[@]}"
"$dir/bin/llmux2-deploy" deploy --config "$dir/llmux-next.toml"
