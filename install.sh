#!/bin/sh
# enishi installer for Linux (x86_64).
#   curl -fsSL https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.sh | sh
# Env: ENISHI_VERSION=v0.1.0 to pin a release (default: newest, pre-releases included).
#      Extra args are passed to `enishi install` (e.g. `| sh -s -- --no-start`).
set -eu

REPO="rauldev-yoru/enishi"
ASSET="enishi-x86_64-unknown-linux-gnu"

say() { printf 'enishi-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "this script is for Linux; on Windows use install.ps1"
case "$(uname -m)" in x86_64|amd64) ;; *) die "unsupported architecture $(uname -m) (only x86_64 for now)";; esac
command -v curl >/dev/null || die "curl is required"
command -v sha256sum >/dev/null || die "sha256sum is required"

tag="${ENISHI_VERSION:-}"
if [ -z "$tag" ]; then
  tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases?per_page=1" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)
  [ -n "$tag" ] || die "could not find a release of $REPO"
fi
base="https://github.com/$REPO/releases/download/$tag"
say "installing enishi $tag"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
cd "$tmp"
curl -fsSL -o "$ASSET" "$base/$ASSET" || die "download failed: $base/$ASSET"
curl -fsSL -o SHA256SUMS "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
grep " $ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1 || die "checksum mismatch for $ASSET"
say "checksum ok"
chmod +x "$ASSET"
./"$ASSET" install "$@"
