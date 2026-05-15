#!/usr/bin/env bash
set -euo pipefail

out_file="${1:-/tmp/starshard_api_surface.txt}"

rg -n "^(pub (struct|enum|trait|type|const|mod|use)|\s*pub (async )?fn )" \
  src/lib.rs \
  src/advanced.rs \
  src/eviction.rs \
  src/core/sync_impl.rs \
  src/core/async_impl.rs \
  src/serde/async_snapshot.rs > "$out_file"

echo "API surface exported to: $out_file"
