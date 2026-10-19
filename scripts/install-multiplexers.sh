#!/bin/sh
# tmux from apt, and pinned zellij 0.45.1 and zmx 0.8.1, so the mux tests run.
# A binary already installed under $HOME/.local/bin is left in place.
set -eu

if ! command -v tmux >/dev/null 2>&1; then
  sudo DEBIAN_FRONTEND=noninteractive apt-get update
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y tmux
fi

bindir="${HOME}/.local/bin"
mkdir -p "$bindir"

arch=$(uname -m)
case "$arch" in
  x86_64) ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "unsupported architecture: $arch" >&2
    exit 1
    ;;
esac

fetch() {
  name=$1
  url=$2
  if [ -x "$bindir/$name" ]; then
    return 0
  fi
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/archive.tar.gz" "$url"
  tar -xzf "$tmp/archive.tar.gz" -C "$tmp"
  if [ ! -f "$tmp/$name" ]; then
    echo "archive did not contain $name" >&2
    exit 1
  fi
  install -m 755 "$tmp/$name" "$bindir/$name"
  rm -rf "$tmp"
}

fetch zellij "https://github.com/zellij-org/zellij/releases/download/v0.45.1/zellij-${arch}-unknown-linux-musl.tar.gz"
fetch zmx "https://zmx.sh/a/zmx-0.8.1-linux-${arch}.tar.gz"

if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$bindir" >>"$GITHUB_PATH"
fi
