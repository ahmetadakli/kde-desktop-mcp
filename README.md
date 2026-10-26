# kde-desktop-mcp

A screenshot-free desktop control MCP server for KDE Plasma (Wayland),
Hyprland and Windows. It lets MCP clients such as Claude Code read the inside
of applications through the accessibility tree and act on elements directly.
Written in Rust; a single executable.

> Verified on a real KDE Plasma desktop. Hyprland was verified in a nested
> session inside KDE. On Windows, keyboard, mouse and screenshots were tried
> under Wine; tree reading (`tree`, `find`, `get_text`, `focus`, `click`) has
> not yet been tried on real Windows. Please share results from
> `tools/mcp_smoke_test.py`.

## How it works

Instead of looking at the screen, Claude reads the elements of a window as a
list like `[55] button "Save"` and acts on an element by its number.

| Layer | KDE Plasma (Wayland) | Hyprland | Windows |
|---|---|---|---|
| Reading | AT-SPI (D-Bus) | AT-SPI (D-Bus) | UI Automation cache requests |
| Element actions | AT-SPI actions, EditableText | AT-SPI actions, EditableText | Invoke, Toggle, ExpandCollapse, SelectionItem, Value patterns |
| Keyboard | `/dev/uinput` | `/dev/uinput` | Win32 `SendInput` |
| Mouse | Absolute uinput pointer | Hyprland `movecursor` + uinput buttons | `SetCursorPos` + `SendInput` |
| Typing text | Direct insertion via AT-SPI into Qt multi-line fields (clipboard untouched); elsewhere in-process clipboard + Ctrl+V, after which the previous clipboard text is put back | `wtype` (clipboard untouched); otherwise clipboard + Ctrl+V as on KDE | `KEYEVENTF_UNICODE` (clipboard untouched) |
| Windows | KWin scripting (D-Bus) | Hyprland socket: `clients`, `focuswindow` | `SetForegroundWindow` |
| Screenshot (fallback) | KWin `ScreenShot2` (D-Bus); else `spectacle` | `grim` | GDI |

The code lives in `rust/src/`: `main.rs` holds the MCP protocol, the
allowlist, element ids and the tools; `render.rs` the tree and search output;
`backend.rs` defines the platform interface. Platform code is in `linux/`
(KDE and Hyprland share an AT-SPI layer) and `windows/`. Claude sees the same
tools on every platform.

The desktop is chosen automatically: Windows on Windows; on Linux, Hyprland if
`HYPRLAND_INSTANCE_SIGNATURE` is set, otherwise KDE. To choose by hand on
Linux, set `DESKTOP_MCP_BACKEND` to `kde` or `hyprland`.

On Hyprland the active window is matched to its AT-SPI application by process
id (pid), so the toolkit's focus flag does not have to be trusted.

`focus` first asks the accessibility layer for focus. If the app refuses (common
with Qt), it clicks the centre of the element for real.

Why it is fast:
- It talks to AT-SPI over D-Bus directly, without libatspi, and pipelines
  requests: all attributes of an element, and every level of the tree, are
  requested at once, so the time is bound by the app's response speed rather
  than by round-trip latency.
- On Windows a window's whole subtree, with the properties needed, arrives in
  a single UI Automation cache request (`BuildUpdatedCache`).
- KWin scripts go over D-Bus instead of `qdbus6` processes, Hyprland commands
  over Hyprland's socket instead of `hyprctl` processes.
- Start-up takes 2 ms; the virtual keyboard and mouse are prepared in parallel
  at start-up.
- On KDE the waits were measured and shortened (uinput events are processed
  in order); `focus` remembers focus methods that do not work in an app and
  skips them; `type_text` starts no process; screenshots come straight from
  KWin instead of `spectacle`.
- Output is short, so Claude reads less: one space of indent per level, sibling
  leaves with the same role and actions share one line
  (`list item ×3: [8] "a" [9] "b" [10] "c"`), and the "Set Focus" action Qt
  gives almost every element is not listed. A Dolphin + Kate `tree` went from
  15,182 to 7,092 characters.
- In browsers too: Firefox's "clickAncestor" action on every element and the
  embedded-object placeholder (U+FFFC) of web pages are not listed; real action
  names are read instead of Firefox's broken batched names (`;;`).
  `tree(document=true)` shows only the active tab's page. The same Firefox page
  went from 5,943 to 1,421 characters.
- An AT-SPI call waits at most 2 s; an app that does not answer cannot lock
  the server. Actions that open a menu (Qt does not reply until the menu
  closes) count as sent after 400 ms.
- Tool calls are handled concurrently: a pending `wait_for` or a slow app does
  not hold up `ping` and other calls. If Claude cancels a call
  (`notifications/cancelled`), it is stopped. Tools that send keyboard or
  pointer input still run one at a time, so keystrokes never interleave.
- Popup menus and overlays of web pages are part of `tree`/`find` output.
  Firefox does not mark these layers as SHOWING; containers that are visible
  and lie inside the window are entered as well, while content scrolled out of
  view is still skipped. Example: Gemini's upload menu ("Dosya yükleyin") is
  now found without a screenshot.
- Read tools (`apps`, `tree`, `find`, `get_text`, `wait_for`, `screenshot`,
  `pause`, `clipboard_get`, `windows`, `doctor`) carry MCP's `readOnlyHint`;
  Claude Code runs several of them in parallel when asked for in one message.

## On-screen indicator

While the agent uses the keyboard or mouse, the screen edges glow orange, a
label at the top says what it is doing (e.g. "Claude · typing") and a ring
marks each pointer target. All of them are layers that pass clicks through to
the window below (wlr-layer-shell; KDE Plasma and Hyprland). They disappear
2.5 s after the last action and are hidden before a screenshot is taken. The
server starts the indicator on the first action as `desktop-mcp --indicator`;
it exits with the server. To turn it off, set the environment variable
`DESKTOP_MCP_INDICATOR=0` in the MCP configuration.

## Tools

- `apps`, `tree`, `find`, `get_text`: reading and searching. With
  `document: true`, `tree` lists only the document of the active window (in a
  browser, that tab's page without tabs and toolbars).
- `set_text` reads back what it wrote: the reply says "verified", or it fails
  saying what the field holds (a length limit, reformatting). Firefox's input
  fields report accessibility writes as successful and do nothing, so when
  single-line text does not take, it focuses the element, selects all with
  Ctrl+A and types it. Password fields cannot be read back.
- `scroll`: turns the wheel over an element (a list, a page, a panel).
- `drag`: presses, moves in steps and releases (drag and drop, sliders,
  selections). Each end is an element id (`from_id`/`to_id`), a screen
  coordinate, or with `image: true` a pixel of the last screenshot.
- `act`, `set_text`, `focus`, `click`: act on an element. The element is picked
  by its `tree`/`find` number (`id`) or in one call by `app` + `query`
  (+ `role`): the first visible element whose name equals `query`, else the
  first whose name or text contains it. Window and tab titles (frame, window,
  page tab) count only when nothing else matches or `role` names that role;
  `wait_for` skips them unless `role` names them. `act` reports when the
  active window changed (e.g. a dialog opened).
- Stale ids: every use of an `id` reads the element's role and name again (one
  round trip). If the element is gone, or its role or name changed (the page
  was redrawn, the object path now belongs to another element), nothing is
  done and the reply is `stale id N (was <role> "<name>", now ...); call
  tree/find again`. For elements whose name follows their content or title
  (windows, tabs, documents, labels, status bars, text fields, combo boxes,
  editable elements) and for unnamed elements, only the role is compared.
- `wait_for`: waits until an element appears or disappears (checks every
  50 ms, at most 10 minutes); for dialogs opened by a key or click, a long
  load or a build. A longer timeout is cut to 10 minutes and the reply says so.
- `pause`: waits a fixed time (`ms`, at most 10 minutes); for waiting on time
  only, not on a visible change.
- `windows`: lists windows of allowed apps with position, size, state (active,
  maximized, minimized) and output in screen coordinates, after the screen
  layout (name, area, scale) (KDE only).
- `move_window`: moves, resizes, maximizes, minimizes or restores a window;
  waits (at most 1 s) until the window has its new geometry and reports the
  real result (KDE only).
- `launch`: starts an allowed app (`app`, e.g. `kate`) with an optional `args`
  list (no shell; its `.desktop` file first, else the program on `PATH`) and
  waits until its window opens (`wait_ms`, default 15 s, at most 60 s; 0 does
  not wait). Prints the window as `windows` does; also finds launches handed
  to an already running instance (firefox, kate). Window tracking is KDE only.
- Terminal windows: `windows` also lists each terminal tab's pts device,
  foreground command and directory; `activate_window` with
  `terminal: "pytest"` brings forward the terminal running that command (or in
  that directory). A terminal with several tabs comes forward on whichever tab
  it shows. The terminal must be in the allowlist (e.g. `konsole`).
- `file_dialog`: fills in the file Open/Save dialog in front: `path` (absolute
  or `~/…`) is written into its file name field, the default button (Open,
  Save) is pressed and the dialog's closing is awaited. Works on an allowed
  app's own KDE dialog and on the portal dialog (`xdg-desktop-portal-kde`) a
  browser opens for uploads and downloads; a portal dialog is filled only when
  the parent window KWin reports for it belongs to an allowed app, and a
  dialog without a reported parent is refused. The portal is not added to the
  allowlist; no other element of the dialog is reachable. For
  `action: "open"` the file must exist, for `"save"` its folder (KDE only).
- `doctor`: checks the setup in one report: version, allowlist, which running
  apps are allowed, the session, AT-SPI, the virtual keyboard and mouse, KWin
  scripting, screens and the screenshot path.
- `key`, `type_text`: keyboard. The reply names the element that has focus
  afterwards; `type_text` warns when focus is not on an editable field (KDE).
- `activate_window`: brings a window to the front; fails when no window of an
  allowed app matches.
- `clipboard_get`, `clipboard_set`: read or write the clipboard as text (Linux
  only). Because the clipboard can hold anything the user copied anywhere
  (passwords included), both work only while a window of an allowed app is
  active. `clipboard_get` returns at most `limit` characters (default 20,000;
  the reply says when it was cut); if the clipboard holds an image or files
  rather than text, it only names the types, and it does not read content a
  password manager marked as a password (`x-kde-passwordManagerHint`).
- `mouse`, `screenshot`: fallback only. `screenshot` returns the image inside
  the reply (no need to read the file separately), shrinks it so its longer
  side is 1280 px by default (`max_size`; `inline: false` returns only the file
  path) and reports which screen area it shows; given `image_x`/`image_y`,
  `mouse` converts a pixel of the image to a screen coordinate (window position
  and display scaling, e.g. 125 %, included).
- `batch`: runs several tools in order in one call and stops at the first
  error; reduces Claude's number of turns.

## Allowlist

Re-read on every call; a default list is created on first run.

- KDE and Hyprland: `~/.config/desktop-mcp/allow.txt`, one AT-SPI application
  name per line (`kate`, `firefox`…). Window tools (`windows`, `move_window`,
  `activate_window`) match a window by the AT-SPI application name of its
  process (pid), so apps whose window class differs are found too (Ferdium:
  `electron`). Apps without accessibility match by window class (like `xterm`).
- Windows: `%APPDATA%\desktop-mcp\allow.txt`, one program name per line without
  `.exe` (`notepad`, `firefox`, `chrome`…).

Rules:
- Elements of apps not in the list cannot be read or acted on.
- Keyboard and mouse input is sent only when the window in front belongs to an
  app in the list.
- `activate_window` only brings windows of listed apps to the front.
- `launch` refuses an app not in the list before starting anything.
- `file_dialog` fills in the dialog of the (unlisted) portal only when it was
  opened for a listed app; a dialog opened by another app is refused.

Use the `apps` tool to see the right names.

## Installation

Every platform needs [Rust](https://rustup.rs/). After installing, open a new
Claude Code session and check with `/mcp` that the `desktop` server is
connected.

### KDE Plasma (Wayland)

Requirements:
- KDE Plasma 6, Wayland session
- System packages (Arch): `at-spi2-core`, `libkscreen`, `wl-clipboard`,
  `spectacle`
- KWin 6.8 or later for fast screenshots. On older KWin, `screenshot` falls
  back to `spectacle`; enabling KWin's `ScreenShot2` interface takes installing
  a `.desktop` file by hand once (details in the `SHOT_NOT_AUTHORIZED` comment
  in `rust/src/linux/kwin.rs`)
- Write access for the user on `/dev/uinput` (`getfacl /dev/uinput`)

```bash
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp/rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD/target/release/desktop-mcp"
```

### Hyprland

Requirements:
- A Hyprland session (the `movecursor` dispatcher)
- System packages (Arch): `at-spi2-core`, `wl-clipboard`; recommended: `wtype`
  (typing without touching the clipboard), `grim` (fallback screenshots)
- A working AT-SPI bus in the session. Qt apps may need
  `env = QT_LINUX_ACCESSIBILITY_ALWAYS_ON,1` in the Hyprland configuration.
- Write access for the user on `/dev/uinput`

The installation commands are the same as for KDE. Start Claude Code from
inside the Hyprland session so `HYPRLAND_INSTANCE_SIGNATURE` reaches the
server.

### Windows

Requirements: Windows 10 or 11. In PowerShell:

```powershell
git clone https://github.com/ahmetadakli/kde-desktop-mcp.git
cd kde-desktop-mcp\rust
cargo build --release
claude mcp add --scope user desktop -- "$PWD\target\release\desktop-mcp.exe"
```

Building for Windows on Linux also works (`mingw-w64-gcc` and
`rustup target add x86_64-pc-windows-gnu`, then
`cargo build --release --target x86_64-pc-windows-gnu`).

### Speed: skipping the permission prompt

Claude Code sends every MCP call that is not in its allow list to a prompt or,
in auto mode, to its safety classifier; that adds roughly 0.7–2.5 s per call.
If you trust all of the server's tools, add one rule to
`~/.claude/settings.json`:

```json
{ "permissions": { "allow": ["mcp__desktop"] } }
```

## Testing

Unit tests need no real desktop (the pure part of the Windows logic runs on
Linux too):

```bash
cd rust && cargo test --release
```

`tools/mcp_smoke_test.py` tests end to end on the real desktop over the real
MCP protocol (Linux, Kate): it opens the editor with a temporary file, finds the
element, refocuses it after focus moved elsewhere, types text with Turkish
characters through `batch`, tries rapid consecutive typing and key repeat,
saves with Ctrl+S, reads the file back from disk, launches Kate with `launch`,
and checks that the allowlist blocks an unknown app. It prints the duration of
every call. Only the Python standard library is needed. Do not touch the
keyboard and mouse while it runs.

```bash
python3 tools/mcp_smoke_test.py rust/target/release/desktop-mcp
```

`tools/mcp_bench.py` measures one or more servers with the same calls and
prints a table of median ms (the window must be open):

```bash
python3 tools/mcp_bench.py --app kate --window notes.txt \
    --server "Rust=rust/target/release/desktop-mcp"
```

## Measurements: KDE speed-up

`tools/mcp_bench.py`, KDE Plasma 6.8, Kate; median of 5 rounds (ms). "Before"
is `2f2e5df` (the version that removed Python), "After" this version:

| Operation | Before (ms) | After (ms) | Speed-up |
|---|---:|---:|---:|
| `initialize` | 4.4 | 3.3 | 1.4x |
| `apps` | 3.8 | 4.2 | 0.9x |
| `activate_window` | 8.5 | 10.3 | 0.8x |
| `tree` | 12.0 | 10.0 | 1.2x |
| `find` | 10.1 | 9.9 | 1.0x |
| `get_text` | 0.3 | 0.3 | 0.9x |
| `key` | 19.5 | 6.4 | 3.0x |
| `type_text` | 34.9 | 2.9 | 12x |
| `focus` | 103.5 | 12.1 | 8.5x |
| `click` | 78.1 | 11.1 | 7.0x |
| `mouse` | 29.3 | 4.9 | 6.0x |
| `screenshot` | 258.8 | 12.1 | 21x |

The ~2 ms in `activate_window` is reading the active window title so `act` can
report a window change. The real gain lies beyond milliseconds: short output,
targeting with `app` + `query` in one call, `wait_for` and `batch` reduce the
number of Claude's turns and the text it reads.

## Measurements: compared with the old Python version

The server used to be written in Python; its last state is commit `4cd2632` in
the git history. The Rust version offers the same tools with the same output:
1,499 lines of output on KDE and 2,025 on Hyprland were compared with the
Python version, with no difference. The values were taken with
`tools/mcp_bench.py` applying the same call sequence to both servers in turn;
median of 5 rounds (ms, in a Kate window, same machine).

KDE Plasma 6 (Wayland):

| Operation | Python (ms) | Rust (ms) | Speed-up |
|---|---:|---:|---:|
| `initialize` | 400.5 | 2.2 | 181x |
| `apps` | 48.8 | 4.2 | 11.6x |
| `activate_window` | 22.7 | 7.4 | 3.1x |
| `tree` | 66.6 | 11.5 | 5.8x |
| `find` | 40.8 | 9.4 | 4.3x |
| `get_text` | 5.0 | 0.3 | 15x |
| `key` | 61.3 | 19.4 | 3.2x |
| `type_text` | 197.0 | 35.6 | 5.5x |
| `focus` | 339.2 | 102.8 | 3.3x |
| `click` | 165.5 | 78.4 | 2.1x |
| `mouse` | 66.4 | 28.8 | 2.3x |
| `screenshot` | 269.0 | 254.3 | 1.1x |

Hyprland 0.56 (nested session inside KDE):

| Operation | Python (ms) | Rust (ms) | Speed-up |
|---|---:|---:|---:|
| `initialize` | 394.5 | 2.2 | 178x |
| `apps` | 145.6 | 4.3 | 34x |
| `activate_window` | 225.0 | 3.1 | 73x |
| `tree` | 120.9 | 18.5 | 6.5x |
| `find` | 75.9 | 16.1 | 4.7x |
| `get_text` | 4.5 | 0.3 | 13x |
| `key` | 60.5 | 19.2 | 3.1x |
| `type_text` | 28.6 | 11.1 | 2.6x |
| `focus` | 319.5 | 77.7 | 4.1x |
| `click` | 168.7 | 78.0 | 2.2x |
| `mouse` | 69.1 | 28.3 | 2.4x |
| `screenshot` | 38.1 | 24.2 | 1.6x |

Windows (Notepad inside Wine 11.20; not a real Windows measurement):

| Operation | Python (ms) | Rust (ms) | Speed-up |
|---|---:|---:|---:|
| `initialize` | 1998.6 | 62.8 | 32x |
| `apps` | 43.0 | 1.4 | 32x |
| `activate_window` | 5.9 | 0.9 | 6.4x |
| `key` | 54.8 | 19.3 | 2.8x |
| `type_text` | 7.1 | 1.1 | 6.5x |
| `mouse` | 55.6 | 30.5 | 1.8x |
| `screenshot` | 82.3 | 8.3 | 9.9x |

Wine's UI Automation client does not support cache requests, tree walking and
most properties (`E_NOTIMPL`), so `tree`, `find`, `get_text`, `focus` and
`click` do not work under Wine on either server; the `apps` and
`activate_window` figures in the table are for returning without finding a
window. Most of the difference in the input steps comes from Rust's shorter
waits.

The gap grows with larger trees: a 2,000-element `tree` of Dolphin including
hidden elements takes 507 ms in Python and 48 ms in Rust. `screenshot` time is
mostly the `spectacle` and `grim` programs themselves.

In nested Hyprland, Ctrl/Alt shortcuts from uinput get lost on their way
through the outer KWin (on both servers), so no shortcut tests were run there.
In a real Hyprland session uinput goes straight to Hyprland.

## Known limitations

KDE:
- The server turns on the accessibility bus at run time. Apps started before
  that need one restart.
- Chromium, Chrome and Electron apps show their tree only when started with
  `--force-renderer-accessibility`.
- `type_text` pastes through the clipboard into fields other than Qt's
  multi-line editors (such as Kate). It first saves the clipboard text and
  puts it back once the target app has read the paste (seen through Wayland
  data-control). The typed text does not enter Klipper's history (it is
  offered with the password hint). The clipboard is not put back when it held
  an image, files or text over 4 MiB, nor when the app does not read the paste
  within 1 s (so that a late reader does not paste the user's clipboard); the
  reply says so in both cases. Rich text (HTML) comes back as plain text only.
- Direct insertion differs slightly from pasting: replacing selected text is
  two undo steps, with multiple cursors only the primary cursor gets the text,
  overwrite mode and Kate's vi mode are bypassed, and auto-indent and bracket
  completion do not run as they would on a paste.

Hyprland:
- Client-side shadows of GTK apps can shift `click` and `focus` clicks by a
  few pixels.
- Two windows of the same process with the same title cannot be told apart.
- Without `wtype`, `type_text` pastes through the clipboard as on KDE and puts
  the clipboard text back afterwards.

Windows:
- Input cannot be sent to windows running as administrator from a server that
  is not (Windows UIPI).
- Microsoft Store apps may show up as `applicationframehost`.
- Key names follow the US keyboard layout positions; use `type_text` for text.
- In very large windows (browsers, Word) `tree` and `find` can be slow because
  they fetch the whole subtree at once.

All:
- Games and canvas-based interfaces mostly show up empty in the accessibility
  tree.

## Security

The server runs with your user's permissions. The allowlist is a safety layer,
not operating-system level isolation. Think twice before adding terminals, the
file manager or system settings to the list.

## License

MIT
