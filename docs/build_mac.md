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

From the repository root:

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
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX14.4.sdk \
CLANG_MODULE_CACHE_PATH=/tmp/clang-module-cache \
bun run tauri build --bundles dmg
```

## Output

- DMG: `src-tauri/target/release/bundle/dmg/Handy_<version>_aarch64.dmg`

To install the app without building a DMG, use `--bundles app` instead and copy
`src-tauri/target/release/bundle/macos/Handy.app` into `/Applications`.

Optional checksum:

```bash
shasum -a 256 src-tauri/target/release/bundle/dmg/Handy_<version>_aarch64.dmg
```

## Notes

- Since the v0.9.4 upstream merge, the app links against ONNX Runtime (`ort`)
  whose prebuilt binary references CoreML symbols (`MLComputePlan`,
  `MLOptimizationHints`) available only in macOS SDK 14.4+. With an older
  Xcode (e.g. 15.1, SDK 14.2), linking fails with
  `Undefined symbols: _OBJC_CLASS_$_MLComputePlan`.
  Fix without touching Xcode: install Command Line Tools 15.3+
  (`softwareupdate -i "Command Line Tools for Xcode-15.3"`, ~700 MB) and
  export the newer SDK for the build:

  ```bash
  export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX14.4.sdk
  ```

  Add this export before `cargo build` / `bun run tauri build` commands
  (verified working with Xcode 15.1's toolchain on macOS 14.3).

- `--bundles dmg` avoids updater artifact signing errors when `TAURI_SIGNING_PRIVATE_KEY` is not set.
- If you need updater artifacts, use the normal build and provide `TAURI_SIGNING_PRIVATE_KEY` (and password if encrypted).
- Notarization is skipped unless Apple notarization env vars are configured.
