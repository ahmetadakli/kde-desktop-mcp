# Claude Code plugins

`.claude-plugin/marketplace.json` at the repository root lists the two
plugins here (README, "Claude Code plugin"):

- `desktop-mcp` (Linux): `scripts/desktop-mcp.sh`, run with `sh`.
- `desktop-mcp-windows`: `scripts/desktop-mcp.ps1`, run with Windows
  PowerShell. A plugin has one command for every platform, hence two plugins.

Each launcher runs a `desktop-mcp` on `PATH`, else downloads the release
archive of its pinned version, checks it against the release's `SHA256SUMS`
and runs it from the plugin's data directory.

A version bump changes `version` in both `plugin.json` files, both
marketplace entries and both launchers along with `rust/Cargo.toml`;
`rust/tests/plugin_package.rs` fails until they match. Claude Code offers
the new version as soon as the bump is on `main`, so the launcher says when
the release is not published yet.
