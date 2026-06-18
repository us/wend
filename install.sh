#!/bin/sh
# wend installer — downloads the latest release binary for your platform.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/us/wend/main/install.sh | sh
#   wget -qO- https://raw.githubusercontent.com/us/wend/main/install.sh | sh
#
# Options (environment variables):
#   WEND_VERSION=v0.1.0           Install a specific version instead of latest
#   WEND_INSTALL_DIR=~/.local/bin Custom install directory
#   GITHUB_TOKEN=ghp_...          Avoid GitHub API rate limits
#
# This installs the lean keyword-only build. Semantic (meaning-based) search is
# an opt-in source build: cargo install --git https://github.com/us/wend \
#   wend-cli --features semantic

set -eu

main() {

REPO="us/wend"
INSTALL_DIR="${WEND_INSTALL_DIR:-/usr/local/bin}"
BINARY="${WEND_BINARY:-wend}"

# --- helpers ----------------------------------------------------------------

BOLD="$(tput bold 2>/dev/null || printf '')"
BLUE="$(tput setaf 4 2>/dev/null || printf '')"
GREEN="$(tput setaf 2 2>/dev/null || printf '')"
RED="$(tput setaf 1 2>/dev/null || printf '')"
RESET="$(tput sgr0 2>/dev/null || printf '')"

info()    { printf '%s==>%s %s\n' "${BLUE}${BOLD}" "${RESET}" "$*"; }
success() { printf '%s==>%s %s\n' "${GREEN}${BOLD}" "${RESET}" "$*"; }
err()     { printf '%serror:%s %s\n' "${RED}${BOLD}" "${RESET}" "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "'$1' is required but not found"; }

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error \
         --proto '=https' --tlsv1.2 --output "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget --https-only --quiet --output-document="$2" "$1"
  else
    err "curl or wget is required"
  fi
}

# --- detect platform → rust target triple (matches release asset names) -----

detect_target() {
  OS="$(uname -s)"
  ARCH="$(uname -m)"

  # Rosetta 2: uname reports x86_64 under emulation on Apple Silicon.
  if [ "$OS" = "Darwin" ] && [ "$ARCH" = "x86_64" ]; then
    if sysctl -n sysctl.proc_translated 2>/dev/null | grep -q '^1$'; then
      info "Rosetta 2 detected — installing native arm64 binary"
      ARCH="arm64"
    fi
  fi

  case "$OS" in
    Darwin)
      case "$ARCH" in
        arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
        x86_64|amd64)  TARGET="x86_64-apple-darwin"  ;;
        *) err "Unsupported macOS arch: $ARCH. Try: cargo install --git https://github.com/${REPO} ${BINARY}-cli" ;;
      esac
      EXT="tar.gz" ;;
    Linux)
      case "$ARCH" in
        # Ship the static musl build for x64 — works on glibc and musl (Alpine).
        x86_64|amd64) TARGET="x86_64-unknown-linux-musl" ;;
        *) err "No prebuilt Linux binary for $ARCH yet. Try: cargo install --git https://github.com/${REPO} ${BINARY}-cli" ;;
      esac
      EXT="tar.gz" ;;
    MINGW*|MSYS*|CYGWIN*)
      TARGET="x86_64-pc-windows-msvc"; EXT="zip" ;;
    *)
      err "Unsupported OS: $OS. Try: cargo install --git https://github.com/${REPO} ${BINARY}-cli" ;;
  esac

  ASSET="${BINARY}-${TARGET}.${EXT}"
}

# --- fetch latest version ---------------------------------------------------

get_version() {
  if [ -n "${WEND_VERSION:-}" ]; then VERSION="$WEND_VERSION"; return; fi

  AUTH=""
  [ -n "${GITHUB_TOKEN:-}" ] && AUTH="Authorization: token ${GITHUB_TOKEN}"
  TMPF="$(mktemp)"
  API="https://api.github.com/repos/${REPO}/releases/latest"
  if command -v curl >/dev/null 2>&1; then
    if [ -n "$AUTH" ]; then curl -fsSL -H "$AUTH" "$API" > "$TMPF" 2>/dev/null || true
    else curl -fsSL "$API" > "$TMPF" 2>/dev/null || true; fi
  else
    if [ -n "$AUTH" ]; then wget --header="$AUTH" -qO "$TMPF" "$API" 2>/dev/null || true
    else wget -qO "$TMPF" "$API" 2>/dev/null || true; fi
  fi

  if grep -q '"rate limit"' "$TMPF" 2>/dev/null; then
    rm -f "$TMPF"; err "GitHub API rate limit exceeded. Set GITHUB_TOKEN or use: WEND_VERSION=v0.1.0"
  fi
  VERSION=$(grep '"tag_name"' "$TMPF" | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  rm -f "$TMPF"
  [ -n "$VERSION" ] || err "Could not determine latest version. Use: WEND_VERSION=v0.1.0"
}

# --- download & install -----------------------------------------------------

install_binary() {
  URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET}"
  TMPD="$(mktemp -d)"
  trap 'rm -rf "$TMPD"' EXIT

  if command -v "$BINARY" >/dev/null 2>&1; then
    info "Upgrading $("$BINARY" --version 2>/dev/null | head -1 || echo current) to ${VERSION}…"
  else
    info "Downloading wend ${VERSION} (${TARGET})…"
  fi
  download "$URL" "${TMPD}/${ASSET}"

  info "Extracting…"
  if [ "$EXT" = "zip" ]; then need unzip; unzip -o "${TMPD}/${ASSET}" -d "$TMPD" >/dev/null
  else tar xzf "${TMPD}/${ASSET}" -C "$TMPD"; fi

  BIN_FILE="$BINARY"; [ "$EXT" = "zip" ] && BIN_FILE="${BINARY}.exe"
  [ -f "${TMPD}/${BIN_FILE}" ] || err "Archive did not contain '${BIN_FILE}'"

  if [ ! -d "$INSTALL_DIR" ]; then
    if [ -w "$(dirname "$INSTALL_DIR")" ]; then mkdir -p "$INSTALL_DIR"; else sudo mkdir -p "$INSTALL_DIR"; fi
  fi

  info "Installing to ${INSTALL_DIR}/${BIN_FILE}…"
  if [ -w "$INSTALL_DIR" ]; then
    mv "${TMPD}/${BIN_FILE}" "${INSTALL_DIR}/${BIN_FILE}"; chmod +x "${INSTALL_DIR}/${BIN_FILE}"
  else
    sudo mv "${TMPD}/${BIN_FILE}" "${INSTALL_DIR}/${BIN_FILE}"; sudo chmod +x "${INSTALL_DIR}/${BIN_FILE}"
  fi

  # No Gatekeeper quarantine bit is set on curl-downloaded files, but clear it
  # defensively on macOS just in case the user pre-fetched via a browser.
  [ "$(uname -s)" = "Darwin" ] && xattr -d com.apple.quarantine "${INSTALL_DIR}/${BIN_FILE}" 2>/dev/null || true

  success "wend ${VERSION} installed to ${INSTALL_DIR}/${BIN_FILE}"
  printf '\n  Run:  %s --help\n  Then: %s index && %s doctor\n\n' "$BINARY" "$BINARY" "$BINARY"

  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
    *) printf '  Note: %s is not in your PATH. Add it with:\n    export PATH="%s:$PATH"\n\n' "$INSTALL_DIR" "$INSTALL_DIR" ;;
  esac
}

detect_target
get_version
install_binary

}

main "$@"
