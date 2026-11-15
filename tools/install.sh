#!/usr/bin/env bash
# Install or update desktop-mcp from the latest GitHub release (Linux x86_64),
# without Rust:
#
#   curl -fsSL https://raw.githubusercontent.com/ahmetadakli/kde-desktop-mcp/main/tools/install.sh | bash
#
# Run it again to update. It downloads the release archive, checks it
# against the release's SHA256SUMS, installs the binary as
# $INSTALL_DIR/desktop-mcp (default ~/.local/bin) and, when Claude Code has
# no `desktop` server yet, adds it at user scope. Environment:
#   VERSION=0.16.0       a given release instead of the latest
#   INSTALL_DIR=…        where the binary goes
#   NO_CLAUDE_ADD=1      do not touch Claude Code's MCP config
#   RELEASES_URL=…       releases base URL (tests)
set -euo pipefail

repo_releases=${RELEASES_URL:-https://github.com/ahmetadakli/kde-desktop-mcp/releases}
install_dir=${INSTALL_DIR:-$HOME/.local/bin}

die() { echo "install.sh: $*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "Linux only; on Windows download the .zip from $repo_releases"
[ "$(uname -m)" = x86_64 ] || die "only x86_64 builds are published; build from source (README, Installation)"
command -v curl >/dev/null || die "curl is needed"
command -v sha256sum >/dev/null || die "sha256sum is needed"

version=${VERSION:-}
if [ -z "$version" ]; then
    # Where releases/latest redirects: …/releases/tag/v0.16.0.
    url=$(curl -sS -o /dev/null -w '%{redirect_url}' "$repo_releases/latest")
    version=${url##*/releases/tag/}
    [ -n "$version" ] && [ "$version" != "$url" ] || die "no release found at $repo_releases"
fi
version=v${version#v}
name=desktop-mcp-$version-x86_64-linux

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
echo "Downloading desktop-mcp $version…"
curl -fsSL -o "$work/$name.tar.gz" "$repo_releases/download/$version/$name.tar.gz" \
    || die "no Linux build in release $version"
curl -fsSL -o "$work/SHA256SUMS" "$repo_releases/download/$version/SHA256SUMS" \
    || die "no SHA256SUMS in release $version"
(cd "$work" && grep " $name.tar.gz\$" SHA256SUMS | sha256sum -c --quiet -) \
    || die "checksum mismatch: not installed"
tar -xzf "$work/$name.tar.gz" -C "$work"

mkdir -p "$install_dir"
target=$install_dir/desktop-mcp
old=$("$target" --version 2>/dev/null || true)
# Replaced in one rename: a running server keeps its old file until restarted.
install -m 755 "$work/$name/desktop-mcp" "$target.new"
mv -f "$target.new" "$target"
echo "Installed $("$target" --version) to $target${old:+ (was ${old#desktop-mcp })}"

if [ "${NO_CLAUDE_ADD:-}" != 1 ] && command -v claude >/dev/null; then
    if claude mcp get desktop >/dev/null 2>&1; then
        current=$(claude mcp get desktop 2>/dev/null | sed -n 's/^ *Command: //p')
        if [ "$current" != "$target" ]; then
            echo "Claude Code's 'desktop' server runs $current, not this binary."
            echo "To use it: claude mcp remove --scope user desktop && claude mcp add --scope user desktop -- $target"
        fi
    else
        claude mcp add --scope user desktop -- "$target"
        echo "Added the 'desktop' MCP server to Claude Code (user scope)."
    fi
fi

case ":$PATH:" in *":$install_dir:"*) ;; *) echo "Note: $install_dir is not on your PATH." ;; esac
echo "Restart Claude Code or reconnect the server (/mcp) to run the new version."
echo "First time? Allow apps in ~/.config/desktop-mcp/allow.txt (README, Allowlist)."
