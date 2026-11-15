# AUR packages

Two package recipes for the [AUR](https://aur.archlinux.org/), tested with
`makepkg` but not published yet (publishing needs an AUR account with an SSH
key):

- `kde-desktop-mcp`: builds the release's source with cargo.
- `kde-desktop-mcp-bin`: installs the prebuilt Linux binary that the release
  workflow attaches to each GitHub release.

Both install `/usr/bin/desktop-mcp`; add it to Claude Code with
`claude mcp add --scope user desktop -- /usr/bin/desktop-mcp`.

## Publishing (first time)

```bash
git clone ssh://aur@aur.archlinux.org/kde-desktop-mcp.git aur-kde-desktop-mcp
cp packaging/aur/kde-desktop-mcp/{PKGBUILD,.SRCINFO} aur-kde-desktop-mcp/
cd aur-kde-desktop-mcp && git add PKGBUILD .SRCINFO && git commit -m "Initial import" && git push
```

The same for `kde-desktop-mcp-bin`.

## For each release

After the release workflow attached the binaries:

1. Set `pkgver` to the new version and `pkgrel=1` in both PKGBUILDs.
2. Update the checksums: `updpkgsums` (pacman-contrib) in each directory;
   for `-bin` they must match the release's `SHA256SUMS`.
3. `makepkg -f` to test, then `makepkg --printsrcinfo > .SRCINFO`.
4. Commit here, and copy both files to the AUR clones and push there.
