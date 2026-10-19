#!/bin/sh
# tmux from apt, and pinned zellij 0.45.1 and zmx 0.8.1 (sha256-checked), so the mux tests run.
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

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d" " -f1
  else
    shasum -a 256 "$1" | cut -d" " -f1
  fi
}

fetch() {
  name=$1
  url=$2
  want=$3
  if [ -x "$bindir/$name" ]; then
    return 0
  fi
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/archive.tar.gz" "$url"
  got=$(sha256_of "$tmp/archive.tar.gz")
  if [ "$got" != "$want" ]; then
    echo "$name: sha256 $got does not match $want" >&2
    rm -rf "$tmp"
    exit 1
  fi
  tar -xzf "$tmp/archive.tar.gz" -C "$tmp"
  if [ ! -f "$tmp/$name" ]; then
    echo "archive did not contain $name" >&2
    exit 1
  fi
  install -m 755 "$tmp/$name" "$bindir/$name"
  rm -rf "$tmp"
}

case "$arch" in
  x86_64)
    zellij_sha=40bcc2e03f5d5ae8e054e39f676081fe12ab70871506996ba595834c3718eefc
    zmx_sha=dfd75720b942466f28870731cc86dbc07afa72fb8f3bd5eeb4ff707e4eecebe8
    ;;
  aarch64)
    zellij_sha=05f0802afadd53f8db9514e7cae53c9ae8432fed1b35b8294aa816ee3044a16b
    zmx_sha=943eb44c812333fd450da12097521afd3339436e86f8c2ac618b905c4c9ece68
    ;;
esac

fetch zellij "https://github.com/zellij-org/zellij/releases/download/v0.45.1/zellij-${arch}-unknown-linux-musl.tar.gz" "$zellij_sha"
fetch zmx "https://zmx.sh/a/zmx-0.8.1-linux-${arch}.tar.gz" "$zmx_sha"

if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$bindir" >>"$GITHUB_PATH"
fi
