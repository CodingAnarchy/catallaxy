#!/bin/sh
# Install a prebuilt `catx` (the Catallaxy toolchain) from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/CodingAnarchy/catallaxy/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version 0.4.0 --prefix /usr/local
#
# Options (or environment variables):
#   --version V   release to install, e.g. 0.4.0 or v0.4.0 (CATX_VERSION; default: the latest)
#   --prefix DIR  installs DIR/bin/catx (CATX_PREFIX; default: ~/.local)
#   --with-capi   also install the C ABI library into DIR/lib and catallaxy.h into DIR/include
#
# CATX_TARGET overrides the detected platform (a Rust target triple), and CATX_DOWNLOAD_URL the
# directory the archives are fetched from (a mirror, or file:// for testing).
set -eu

repo="CodingAnarchy/catallaxy"
version="${CATX_VERSION:-}"
prefix="${CATX_PREFIX:-${HOME:?HOME is not set}/.local}"
capi=no

say() { printf 'install.sh: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version="$2"; shift 2 ;;
    --version=*) version="${1#*=}"; shift ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a value"; prefix="$2"; shift 2 ;;
    --prefix=*) prefix="${1#*=}"; shift ;;
    --with-capi) capi=yes; shift ;;
    -h|--help)
      echo "usage: install.sh [--version V] [--prefix DIR] [--with-capi]"
      echo "Installs DIR/bin/catx (default DIR: ~/.local) from the latest or the given release."
      exit 0 ;;
    *) die "unknown option: $1 (try --help)" ;;
  esac
done

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  die "need curl or wget"
fi

# The platform, as the Rust target triple the release archives are named by.
target="${CATX_TARGET:-}"
if [ -z "$target" ]; then
  os=$(uname -s)
  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) die "no prebuilt catx for $arch; install with: cargo install catallaxy" ;;
  esac
  case "$os" in
    Linux) target="$arch-unknown-linux-gnu" ;;
    Darwin)
      # A shell under Rosetta reports x86_64 on Apple silicon: prefer the native build.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
      fi
      target="$arch-apple-darwin" ;;
    MINGW*|MSYS*|CYGWIN*) die "on Windows, download the .zip from https://github.com/$repo/releases" ;;
    *) die "no prebuilt catx for $os; install with: cargo install catallaxy" ;;
  esac
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t catx)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

if [ -z "$version" ]; then
  # The latest release's tag, from the API (no jq needed: pick out "tag_name").
  fetch "https://api.github.com/repos/$repo/releases/latest" "$tmp/latest.json" ||
    die "could not look up the latest release; pass --version"
  version=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
  [ -n "$version" ] || die "could not find the latest release's version; pass --version"
fi
version="${version#v}"

base="${CATX_DOWNLOAD_URL:-https://github.com/$repo/releases/download/v$version}"
name="catx-$version-$target"
archive="$name.tar.gz"

say "downloading $archive"
fetch "$base/$archive" "$tmp/$archive" ||
  die "could not download $base/$archive (is there a $version release for $target?)"
fetch "$base/$archive.sha256" "$tmp/$archive.sha256" || die "could not download $archive.sha256"

expected=$(cut -d' ' -f1 < "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "$tmp/$archive" | cut -d' ' -f1)
else
  die "need sha256sum or shasum to verify the download"
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] ||
  die "checksum mismatch for $archive (expected $expected, got $actual)"

tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$prefix/bin"
# Copy then rename, so a running catx is never left half-written.
cp "$tmp/$name/catx" "$prefix/bin/.catx.new"
chmod 755 "$prefix/bin/.catx.new"
mv -f "$prefix/bin/.catx.new" "$prefix/bin/catx"
say "installed $prefix/bin/catx ($version, $target)"

if [ "$capi" = yes ]; then
  mkdir -p "$prefix/lib" "$prefix/include"
  cp "$tmp/$name"/lib/* "$prefix/lib/"
  cp "$tmp/$name/include/catallaxy.h" "$prefix/include/"
  say "installed the C ABI library into $prefix/lib and catallaxy.h into $prefix/include"
fi

case ":${PATH:-}:" in
  *":$prefix/bin:"*) ;;
  *) say "$prefix/bin is not on your PATH; add it, e.g.: export PATH=\"$prefix/bin:\$PATH\"" ;;
esac
