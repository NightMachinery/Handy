# Build macOS release

This guide builds a signed local macOS release DMG for Handy.

## Quick path: build, install, and link the CLI

```bash
bun run install:macos
```

`scripts/install-macos.sh` does the whole loop: builds the release bundle with
the SDK and `xattr` workarounds below applied automatically, quits a running
Handy, backs the old app up to `/Applications/Handy-previous.app`, installs the
new one, symlinks the `handy` CLI into `~/bin`, and waits for the app to answer
on its control socket.

The symlink is the part worth doing every time. The CLI binary lives inside the
bundle at `Handy.app/Contents/MacOS/handy`, which is on nobody's PATH, so
installing without linking leaves `handy file.wav` present but unreachable.

Useful flags:

```bash
bash scripts/install-macos.sh --no-build       # install an already-built bundle
bash scripts/install-macos.sh --link-only      # just refresh the CLI symlink
bash scripts/install-macos.sh --setup-signing  # create the signing identity (once)
```

## Keeping macOS permissions across reinstalls

Run this once:

```bash
bash scripts/install-macos.sh --setup-signing
```

Without it, `tauri.conf.json` signs ad-hoc (`signingIdentity: "-"`) and the
app's designated requirement is its code hash:

```
# designated => cdhash H"389ae20f..."
```

That hash changes on every build, so macOS TCC sees each install as a
different application and resets Accessibility and Microphone permissions —
you re-grant them after every rebuild.

`--setup-signing` creates a self-signed code signing certificate named
"Handy Local Signing" in your login keychain and adds a Code Signing trust
setting (macOS will prompt for your password — this is the step that needs it;
`security find-identity` only lists _trusted_ identities, so an untrusted
certificate is invisible to the build). Afterwards the designated requirement
becomes the certificate's leaf hash, which is stable for the certificate's
ten-year lifetime, and permissions persist.

The identity is passed to the build as a `--config` override rather than
committed to `tauri.conf.json`, so a fresh checkout without the certificate
still builds — it just falls back to ad-hoc and says so. Override the name with
`HANDY_SIGN_IDENTITY` if you already have a certificate you prefer.

Note this is a _local_ identity, not an Apple Developer one: it does nothing
for Gatekeeper or for distributing the app to anyone else. Its only job is to
give your own machine a stable identity to remember permissions against.

`HANDY_APP_DIR` and `HANDY_BIN_DIR` override the destinations. The script never
replaces a regular file at the symlink target — if you keep your own `handy`
wrapper there, it stops and tells you rather than destroying it.

The rest of this document is the manual procedure, and explains why the
workarounds exist.

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
