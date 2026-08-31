#!/bin/sh
# aiu installer — curl -fsSL https://aiu.sh/install | sh
#
# Detects OS and architecture, downloads the matching release artifact,
# verifies it against the release's SHA256SUMS, and installs `aiu` into PATH.
# Nothing is written until verification passes, and nothing under the data
# directory or the collection schedule is touched, so this doubles as the
# upgrade path.
#
# Environment:
#   AIU_VERSION         install this version instead of the latest
#   AIU_INSTALL_DIR     where to put the binary (default ~/.local/bin)
#   AIU_BASE_URL        release tree to fetch from
#   AIU_UNAME_S/_M      override platform detection (kernel / machine)
#   AIU_PUBKEY          minisign public key to verify SHA256SUMS against
#   AIU_SKIP_SIGNATURE  proceed when a published signature cannot be checked

set -eu

REPO_URL="https://github.com/felipearosr/ai-usage"
BASE_URL="${AIU_BASE_URL:-$REPO_URL/releases}"
INSTALL_DIR="${AIU_INSTALL_DIR:-${HOME:-.}/.local/bin}"
# The minisign public key releases are signed with. Empty until a release
# signing key exists; an unsigned release publishes no .minisig, so the
# signature path below stays dormant rather than silently passing.
PUBKEY="${AIU_PUBKEY:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'aiu: %s\n' "$*" >&2; exit 1; }

# Downloads a URL to a file, failing on any HTTP error rather than saving the
# error page. file:// URLs work the same way, which is how this is tested.
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget to download $1"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    die "need sha256sum or shasum to verify the download"
  fi
}

# uname output -> Rust target triple. An unrecognized pair is refused by name:
# installing the wrong architecture's binary fails later and less clearly.
detect_target() {
  os="${AIU_UNAME_S:-$(uname -s)}"
  arch="${AIU_UNAME_M:-$(uname -m)}"
  case "$os" in
    Linux)  os_part="unknown-linux-musl" ;;
    Darwin) os_part="apple-darwin" ;;
    *)      die "unsupported operating system: $os (aiu supports Linux and macOS)" ;;
  esac
  case "$arch" in
    x86_64|amd64)  arch_part="x86_64" ;;
    aarch64|arm64) arch_part="aarch64" ;;
    *)             die "unsupported architecture: $arch (aiu supports x86_64 and aarch64)" ;;
  esac
  printf '%s-%s' "$arch_part" "$os_part"
}

resolve_version() {
  if [ -n "${AIU_VERSION:-}" ]; then
    printf '%s' "$AIU_VERSION"
    return
  fi
  fetch "$BASE_URL/latest/download/VERSION" "$tmp/VERSION" \
    || die "could not determine the latest version from $BASE_URL"
  version=$(tr -d ' \t\r\n' < "$tmp/VERSION")
  [ -n "$version" ] || die "the published latest version is empty"
  printf '%s' "$version"
}

# Verifies the artifact against the release checksums. An artifact with no
# line in SHA256SUMS is refused: there is nothing to compare it against, and
# an unvouched-for download is exactly what verification exists to catch.
verify_checksum() {
  # Exact filename match on the second field; a `grep` pattern would let the
  # dots in a version number stand in for any character.
  expected=$(awk -v f="$1" '{ n=$2; sub(/^\*/, "", n); if (n == f) { print $1; exit } }' \
    "$tmp/SHA256SUMS")
  [ -n "$expected" ] || die "$1 is not listed in SHA256SUMS; refusing to install it"
  actual=$(sha256_of "$tmp/$1")
  [ "$expected" = "$actual" ] \
    || die "checksum mismatch for $1 (expected $expected, got $actual); refusing to install"
}

# A signature is published or it is not. When it is, failing to check it is a
# refusal rather than a warning — a warning printed inside `curl | sh` scrolls
# past exactly when it matters most.
verify_signature() {
  fetch "$release_url/SHA256SUMS.minisig" "$tmp/SHA256SUMS.minisig" 2>/dev/null || return 0
  if [ -n "${AIU_SKIP_SIGNATURE:-}" ]; then
    say "aiu: skipping signature verification (AIU_SKIP_SIGNATURE is set)"
    return 0
  fi
  [ -n "$PUBKEY" ] \
    || die "this release is signed but this installer carries no trusted key; supply one with AIU_PUBKEY, or re-run with AIU_SKIP_SIGNATURE=1 to rely on the checksum alone"
  command -v minisign >/dev/null 2>&1 \
    || die "this release is signed but minisign is not installed; install it, or re-run with AIU_SKIP_SIGNATURE=1 to rely on the checksum alone"
  # A signature that is present and checkable but wrong is an attack signal,
  # not an inconvenience: no opt-out is offered here.
  minisign -V -P "$PUBKEY" -m "$tmp/SHA256SUMS" >/dev/null 2>&1 \
    || die "signature verification failed for SHA256SUMS; refusing to install"
}

main() {
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/aiu-install.XXXXXX")
  trap 'rm -rf "$tmp"' EXIT INT TERM

  target=$(detect_target)
  version=$(resolve_version)
  release_url="$BASE_URL/download/v$version"
  archive="aiu-$version-$target.tar.gz"

  say "aiu: installing $version ($target)"
  fetch "$release_url/$archive" "$tmp/$archive" \
    || die "could not download $release_url/$archive"
  fetch "$release_url/SHA256SUMS" "$tmp/SHA256SUMS" \
    || die "could not download the checksums for $version; refusing to install unverified"

  verify_checksum "$archive"
  verify_signature

  tar -xzf "$tmp/$archive" -C "$tmp" || die "could not unpack $archive"
  [ -f "$tmp/aiu" ] || die "$archive does not contain an aiu binary"
  chmod +x "$tmp/aiu"

  mkdir -p "$INSTALL_DIR"
  # Stage inside the destination directory, then rename. A rename within one
  # filesystem is atomic, so the collection schedule firing mid-install sees
  # either the old binary or the new one and never a half-written file —
  # whereas moving straight from $tmp usually crosses filesystems, which
  # degrades to a copy. Renaming also replaces a binary that is currently
  # running, which copying over it would not.
  staged="$INSTALL_DIR/.aiu.install.$$"
  cp "$tmp/aiu" "$staged" || die "could not write to $INSTALL_DIR"
  chmod +x "$staged"
  mv -f "$staged" "$INSTALL_DIR/aiu" \
    || { rm -f "$staged"; die "could not install into $INSTALL_DIR"; }

  say "aiu: installed $INSTALL_DIR/aiu"
  case ":${PATH:-}:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "aiu: $INSTALL_DIR is not on your PATH — add it with:"
       say "       export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
  esac
  say "aiu: next, run \`aiu init\` (first machine) or \`aiu join <code>\`"
}

main "$@"
