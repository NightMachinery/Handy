# Build macOS release

This guide builds a signed local macOS release DMG for Handy.

## Prerequisites

- Rust (stable) and Bun installed
- Xcode + command line tools installed (`xcode-select --install`)
- Dependencies installed in repo root:

```bash
bun install
```

## Build command (DMG)

From `/Users/evar/code/misc/Handy`:

```bash
mkdir -p /tmp/clang-module-cache /tmp/fakebin

cat > /tmp/fakebin/xattr <<'SH'
#!/bin/bash
set -euo pipefail
if [[ "${1:-}" == "-cr" || "${1:-}" == "-rc" ]]; then
  shift
  for path in "$@"; do
    if [[ -d "$path" ]]; then
      /usr/bin/find "$path" -exec /usr/bin/xattr -c {} +
    else
      /usr/bin/xattr -c "$path"
    fi
  done
  exit 0
fi
exec /usr/bin/xattr "$@"
SH
chmod +x /tmp/fakebin/xattr

PATH=/tmp/fakebin:$PATH \
CLANG_MODULE_CACHE_PATH=/tmp/clang-module-cache \
bun run tauri build --bundles dmg
```

## Output

- DMG: `/Users/evar/code/misc/Handy/src-tauri/target/release/bundle/dmg/Handy_0.7.2_aarch64.dmg`

Optional checksum:

```bash
shasum -a 256 /Users/evar/code/misc/Handy/src-tauri/target/release/bundle/dmg/Handy_0.7.2_aarch64.dmg
```

## Notes

- `--bundles dmg` avoids updater artifact signing errors when `TAURI_SIGNING_PRIVATE_KEY` is not set.
- If you need updater artifacts, use the normal build and provide `TAURI_SIGNING_PRIVATE_KEY` (and password if encrypted).
- Notarization is skipped unless Apple notarization env vars are configured.
