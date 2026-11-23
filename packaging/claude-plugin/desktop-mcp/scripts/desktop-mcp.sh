#!/bin/sh
# Starts desktop-mcp for the Claude Code plugin (Linux).
#
# A desktop-mcp on PATH (install.sh, an AUR package, a cargo install) runs as
# is. Otherwise the release build of this plugin's version is downloaded
# once into a per-user cache, checked against the release's SHA256SUMS and
# run from there: an archive that does not match is deleted, never run.
# stdout carries the MCP stream, so every message goes to stderr.
#
# DESKTOP_MCP_RELEASES_URL replaces the GitHub releases URL (tests).
set -eu

# Kept equal to rust/Cargo.toml by rust/tests/plugin_package.rs.
version=0.18.0
releases=${DESKTOP_MCP_RELEASES_URL:-https://github.com/ahmetadakli/kde-desktop-mcp/releases}

say() {
    echo "desktop-mcp plugin: $*" >&2
}

if found=$(command -v desktop-mcp); then
    exec "$found" "$@"
fi

[ "$(uname -s)" = Linux ] || { say "Linux only; on Windows install the desktop-mcp-windows plugin"; exit 1; }
[ "$(uname -m)" = x86_64 ] || { say "only x86_64 builds are published; build from source (README, Installation)"; exit 1; }

# Claude Code passes CLAUDE_PLUGIN_DATA: kept across plugin updates, removed
# with the plugin.
root=${CLAUDE_PLUGIN_DATA:-${XDG_CACHE_HOME:-$HOME/.cache}/desktop-mcp/plugin}
dir=$root/v$version
bin=$dir/desktop-mcp

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        say "curl or wget is needed to download desktop-mcp"
        return 1
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1"
    else
        shasum -a 256 "$1"
    fi | cut -d' ' -f1
}

install_release() {
    name=desktop-mcp-v$version-x86_64-linux
    url=$releases/download/v$version
    # Private, as the server wants its desktop-mcp directory under ~/.cache
    # for screenshots and tree files when XDG_RUNTIME_DIR is not usable.
    (umask 077 && mkdir -p "$root")
    tmp=$(mktemp -d "$root/.download.XXXXXX")
    trap 'rm -rf "$tmp"' EXIT
    say "downloading $name.tar.gz"
    # A version bump reaches the plugin before its release is published.
    fetch "$url/$name.tar.gz" "$tmp/$name.tar.gz" \
        || { say "could not download release v$version; it may not be published yet"; exit 1; }
    fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS" \
        || { say "release v$version has no SHA256SUMS; not run"; exit 1; }
    want=$(awk -v f="$name.tar.gz" '$2 == f || $2 == "*" f { print tolower($1); exit }' "$tmp/SHA256SUMS")
    have=$(sha256 "$tmp/$name.tar.gz")
    if [ -z "$want" ] || [ "$want" != "$have" ]; then
        say "checksum mismatch for $name.tar.gz (SHA256SUMS: ${want:-no entry}, file: $have); not run"
        exit 1
    fi
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp" "$name/desktop-mcp"
    chmod 755 "$tmp/$name/desktop-mcp"
    # One rename: a second server starting at the same time never runs a
    # half-written file.
    mkdir -p "$dir"
    mv -f "$tmp/$name/desktop-mcp" "$bin"
    rm -rf "$tmp"
    trap - EXIT
}

[ -x "$bin" ] || install_release

# Another Claude Code session may still run an older plugin version: drop
# versions unused for 30 days and staging left by a killed download.
touch "$dir"
find "$root" -mindepth 1 -maxdepth 1 \
    \( \( -name 'v*' ! -name "v$version" -mtime +30 \) -o \( -name '.download.*' -mmin +60 \) \) \
    -exec rm -rf {} + 2>/dev/null || true

exec "$bin" "$@"
