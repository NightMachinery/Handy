#!/usr/bin/env bash
#
# Build and install Handy on macOS, and put the `handy` CLI on your PATH.
#
# The CLI lives inside the app bundle (Handy.app/Contents/MacOS/handy), which
# is not on anyone's PATH, so a symlink is what makes `handy file.wav` usable.
# Installing without it leaves the connected CLI technically present and
# practically unreachable, which is why this is one script rather than two.
#
#   bash scripts/install-macos.sh                  build, install, link
#   bash scripts/install-macos.sh --no-build       install an already-built bundle
#   bash scripts/install-macos.sh --link-only      just refresh the CLI symlink
#   bash scripts/install-macos.sh --setup-signing  create the local signing identity
#
# Environment overrides:
#   HANDY_APP_DIR        where the bundle goes      (default /Applications)
#   HANDY_BIN_DIR        where the CLI symlink goes (default ~/bin)
#   HANDY_SIGN_IDENTITY  code signing identity      (default "Handy Local Signing")
#   SDKROOT              macOS SDK for the build    (auto-detected)
#
set -euo pipefail

APP_DIR="${HANDY_APP_DIR:-/Applications}"
BIN_DIR="${HANDY_BIN_DIR:-$HOME/bin}"
APP_NAME="Handy.app"
CLI_NAME="handy"
SIGN_IDENTITY="${HANDY_SIGN_IDENTITY:-Handy Local Signing}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE="$REPO_ROOT/src-tauri/target/release/bundle/macos/$APP_NAME"

DO_BUILD=1
DO_INSTALL=1
DO_SETUP_SIGNING=0

for arg in "$@"; do
  case "$arg" in
    --no-build)  DO_BUILD=0 ;;
    --link-only) DO_BUILD=0; DO_INSTALL=0 ;;
    --setup-signing) DO_SETUP_SIGNING=1; DO_BUILD=0; DO_INSTALL=0 ;;
    -h|--help)
      # Print the header comment block, stopping at the first non-comment line.
      awk 'NR>1 && /^#/ {sub(/^# ?/, ""); print; next} NR>1 {exit}' "${BASH_SOURCE[0]}"
      exit 0 ;;
    *) echo "install-macos: unknown option '$arg' (try --help)" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "this script is macOS-only"

has_identity() {
  security find-identity -v -p codesigning 2>/dev/null | grep -qF "$SIGN_IDENTITY"
}

# ---------------------------------------------------------------------------
# Signing identity
# ---------------------------------------------------------------------------
#
# Without a stable identity, tauri signs ad-hoc and the app's designated
# requirement is its cdhash — which changes on every build. macOS TCC keys on
# that requirement, so every install looks like a brand new application and
# Accessibility and Microphone permissions reset. A self-signed certificate
# fixes it: the requirement becomes the certificate's leaf hash, which is
# stable for the life of the cert.

setup_signing() {
  if has_identity; then
    say "Signing identity '$SIGN_IDENTITY' is already usable"
    return 0
  fi

  local keychain="$HOME/Library/Keychains/login.keychain-db"
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  if ! security find-certificate -c "$SIGN_IDENTITY" >/dev/null 2>&1; then
    say "Creating a self-signed code signing certificate: $SIGN_IDENTITY"
    openssl req -x509 -newkey rsa:4096 -keyout "$tmp/key.pem" -out "$tmp/cert.pem" \
      -days 3650 -nodes -subj "/CN=$SIGN_IDENTITY" \
      -addext "basicConstraints=critical,CA:FALSE" \
      -addext "keyUsage=critical,digitalSignature" \
      -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null

    # macOS Security cannot verify OpenSSL 3's default PKCS#12 MAC, so pin the
    # legacy algorithms it does understand.
    openssl pkcs12 -export -out "$tmp/id.p12" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
      -name "$SIGN_IDENTITY" -passout pass:handy \
      -legacy -macalg sha1 -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES 2>/dev/null

    security import "$tmp/id.p12" -k "$keychain" -P handy \
      -T /usr/bin/codesign -T /usr/bin/security >/dev/null
  else
    say "Certificate exists but is not trusted for code signing"
  fi

  # find-identity only lists trusted identities, so the trust setting is what
  # makes the certificate usable. This prompts for your login password.
  say "Adding a Code Signing trust setting (macOS will ask for your password)"
  security find-certificate -c "$SIGN_IDENTITY" -p > "$tmp/cert.pem"
  security add-trusted-cert -r trustRoot -p codeSign -k "$keychain" "$tmp/cert.pem"

  has_identity || die "identity still not usable; check Keychain Access for '$SIGN_IDENTITY'"
  say "Signing identity ready"
}

if [ "$DO_SETUP_SIGNING" = 1 ]; then
  setup_signing
  say "Done. Re-run without --setup-signing to build and install."
  exit 0
fi

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

if [ "$DO_BUILD" = 1 ]; then
  say "Building the release bundle"

  # Since the v0.9.4 upstream merge, ONNX Runtime references CoreML symbols
  # (MLComputePlan, MLOptimizationHints) that need SDK 14.4+. Xcode 15.1 ships
  # 14.2, so point at a newer SDK when one is installed. See docs/build_mac.md.
  if [ -z "${SDKROOT:-}" ]; then
    for sdk in /Library/Developer/CommandLineTools/SDKs/MacOSX*.sdk; do
      [ -d "$sdk" ] || continue
      SDKROOT="$sdk"
    done
    [ -n "${SDKROOT:-}" ] && export SDKROOT && say "Using SDKROOT=$SDKROOT"
  fi

  # tauri's bundler calls `xattr -cr`, which the stock xattr does not accept.
  # Shim it onto PATH for the duration of the build.
  FAKEBIN="$(mktemp -d)"
  cat > "$FAKEBIN/xattr" <<'SH'
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
  chmod +x "$FAKEBIN/xattr"
  trap 'rm -rf "$FAKEBIN"' EXIT

  # Prefer a stable signing identity. Passed as a config override rather than
  # committed to tauri.conf.json, so a checkout without this certificate still
  # builds (ad-hoc) instead of failing.
  SIGN_ARGS=()
  if has_identity; then
    say "Signing as '$SIGN_IDENTITY' — macOS permissions survive reinstalls"
    SIGN_ARGS=(--config "{\"bundle\":{\"macOS\":{\"signingIdentity\":\"$SIGN_IDENTITY\"}}}")
  else
    warn "No '$SIGN_IDENTITY' identity found; signing ad-hoc.
    Every build will get a new code hash, so macOS will treat it as a new app
    and reset Accessibility and Microphone permissions on each install.
    Fix it once with: bash scripts/install-macos.sh --setup-signing"
  fi

  # The bundler exits non-zero after a successful bundle when no updater
  # signing key is set, so the bundle on disk — not the exit code — is the
  # real success signal.
  set +e
  ( cd "$REPO_ROOT" && \
    PATH="$FAKEBIN:$PATH" \
    CLANG_MODULE_CACHE_PATH="${CLANG_MODULE_CACHE_PATH:-$(mktemp -d)}" \
    CMAKE_POLICY_VERSION_MINIMUM="${CMAKE_POLICY_VERSION_MINIMUM:-3.5}" \
    bun run tauri build --bundles app "${SIGN_ARGS[@]}" )
  build_status=$?
  set -e

  if [ ! -d "$BUNDLE" ]; then
    die "build failed (exit $build_status) and no bundle at $BUNDLE"
  fi
  if [ "$build_status" -ne 0 ]; then
    say "Bundler exited $build_status after bundling (expected without TAURI_SIGNING_PRIVATE_KEY)"
  fi
fi

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------

if [ "$DO_INSTALL" = 1 ]; then
  [ -d "$BUNDLE" ] || die "no bundle at $BUNDLE — run without --no-build first"

  TARGET_APP="$APP_DIR/$APP_NAME"

  # A running bundle cannot be replaced cleanly. Match on the full path: the
  # process is named `handy`, not `Handy`, so `pgrep -x Handy` finds nothing
  # and would let this proceed against a live app.
  if pgrep -f "$TARGET_APP/Contents/MacOS/" >/dev/null 2>&1; then
    say "Quitting the running Handy"
    osascript -e 'quit app "Handy"' >/dev/null 2>&1 || true
    for _ in $(seq 1 15); do
      pgrep -f "$TARGET_APP/Contents/MacOS/" >/dev/null 2>&1 || break
      sleep 1
    done
    pgrep -f "$TARGET_APP/Contents/MacOS/" >/dev/null 2>&1 && \
      die "Handy is still running; quit it and re-run"
  fi

  if [ -d "$TARGET_APP" ]; then
    BACKUP="$APP_DIR/Handy-previous.app"
    say "Backing up the current app to $BACKUP"
    rm -rf "$BACKUP"
    mv "$TARGET_APP" "$BACKUP"
  fi

  say "Installing to $TARGET_APP"
  mkdir -p "$APP_DIR"
  cp -R "$BUNDLE" "$TARGET_APP"
  # Locally built bundles are not quarantined, but a copied-in one may be.
  command xattr -d -r com.apple.quarantine "$TARGET_APP" >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# CLI symlink
# ---------------------------------------------------------------------------

CLI_SOURCE="$APP_DIR/$APP_NAME/Contents/MacOS/$CLI_NAME"
CLI_LINK="$BIN_DIR/$CLI_NAME"

[ -x "$CLI_SOURCE" ] || die "no CLI binary at $CLI_SOURCE"

mkdir -p "$BIN_DIR"

# Only ever replace a symlink. A regular file here is something the user put
# there — a wrapper script, say — and clobbering it silently would destroy
# work with no way to recover it.
if [ -e "$CLI_LINK" ] && [ ! -L "$CLI_LINK" ]; then
  die "$CLI_LINK exists and is not a symlink; move it aside and re-run"
fi

if [ -L "$CLI_LINK" ] && [ "$(readlink "$CLI_LINK")" = "$CLI_SOURCE" ]; then
  say "CLI symlink already correct: $CLI_LINK"
else
  say "Linking $CLI_LINK -> $CLI_SOURCE"
  ln -sfn "$CLI_SOURCE" "$CLI_LINK"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not on your PATH; add it, e.g.:
    echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

if [ "$DO_INSTALL" = 1 ]; then
  say "Launching Handy"
  open -a "$APP_DIR/$APP_NAME"

  # The control socket comes up during app startup, so give it a moment.
  for _ in $(seq 1 25); do
    if "$CLI_SOURCE" ping >/dev/null 2>&1; then
      say "$("$CLI_SOURCE" ping)"
      say "Done. Try: handy --help"
      exit 0
    fi
    sleep 1
  done
  warn "Handy did not answer on the CLI control socket within 25s.
    The app may still be starting, or the socket may be disabled (--no-ipc).
    Check with: handy status"
else
  say "Done. Try: handy --help"
fi
