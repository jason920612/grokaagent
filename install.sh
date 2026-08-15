#!/bin/sh
# One-line: curl -fsSL https://github.com/jason920612/grokaagent/releases/latest/download/install.sh | sh
set -eu

REPO="${GROKA_UPDATE_REPO:-jason920612/grokaagent}"
INSTALL_DIR="${GROKA_INSTALL_DIR:-$HOME/.grokaagent/bin}"
BIN_NAME="grokaagent"
BASE="https://github.com/${REPO}/releases/latest/download"

os=$(uname -s)
arch=$(uname -m)
case "${os}:${arch}" in
  Linux:x86_64|Linux:amd64)
    TARGET="x86_64-unknown-linux-gnu"
    ;;
  Darwin:arm64|Darwin:aarch64)
    TARGET="aarch64-apple-darwin"
    ;;
  *)
    echo "unsupported platform ${os} ${arch}" >&2
    echo "this installer covers Linux amd64 and macOS Apple Silicon." >&2
    exit 1
    ;;
esac

ASSET="grokaagent-${TARGET}"

if command -v curl >/dev/null 2>&1; then
  download() {
    curl -fsSL "$1" -o "$2"
  }
elif command -v wget >/dev/null 2>&1; then
  download() {
    wget -q -O "$2" "$1"
  }
else
  echo "need curl or wget" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  file_sha256() {
    sha256sum "$1" | awk '{print $1}'
  }
elif command -v shasum >/dev/null 2>&1; then
  file_sha256() {
    shasum -a 256 "$1" | awk '{print $1}'
  }
else
  echo "need sha256sum or shasum" >&2
  exit 1
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/grokaagent.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

echo "downloading grokaagent (${ASSET})…"
download "${BASE}/SHA256SUMS" "${TMP}/SHA256SUMS"
download "${BASE}/${ASSET}" "${TMP}/${ASSET}"

expected=$(awk -v f="$ASSET" '
  $2 == f || $2 == ("*" f) { print $1; exit }
' "${TMP}/SHA256SUMS" | tr 'A-F' 'a-f')
if [ -z "$expected" ]; then
  echo "SHA256SUMS has no entry for ${ASSET}" >&2
  exit 1
fi

actual=$(file_sha256 "${TMP}/${ASSET}" | tr 'A-F' 'a-f')
if [ "$actual" != "$expected" ]; then
  echo "checksum mismatch for ${ASSET}" >&2
  echo "  expected ${expected}" >&2
  echo "  got      ${actual}" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
dest="${INSTALL_DIR}/${BIN_NAME}"
tmpdest="${INSTALL_DIR}/.grokaagent.new"
cp "${TMP}/${ASSET}" "$tmpdest"
chmod 755 "$tmpdest"
mv "$tmpdest" "$dest"

path_has_install_dir() {
  case ":$PATH:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
    *) return 1 ;;
  esac
}

append_path_line() {
  rc="$1"
  line="export PATH=\"${INSTALL_DIR}:\$PATH\""
  if [ -f "$rc" ] && grep -F "$INSTALL_DIR" "$rc" >/dev/null 2>&1; then
    return 0
  fi
  printf '\n# grokaagent\n%s\n' "$line" >> "$rc"
  echo "added ${INSTALL_DIR} to PATH in ${rc}"
}

if ! path_has_install_dir; then
  shell_name=$(basename "${SHELL:-sh}")
  case "$shell_name" in
    zsh)
      if [ -f "$HOME/.zshrc" ] || [ ! -f "$HOME/.zprofile" ]; then
        append_path_line "$HOME/.zshrc"
      else
        append_path_line "$HOME/.zprofile"
      fi
      ;;
    bash)
      if [ -f "$HOME/.bashrc" ]; then
        append_path_line "$HOME/.bashrc"
      else
        append_path_line "$HOME/.bash_profile"
      fi
      ;;
    *)
      append_path_line "$HOME/.profile"
      ;;
  esac
  echo "this shell: export PATH=\"${INSTALL_DIR}:\$PATH\""
fi

echo "installed ${dest}"
echo "open a project directory and run: grokaagent"
echo "later: grokaagent update   (TUI also auto-updates from GitHub Releases)"
