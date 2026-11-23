# Starts desktop-mcp for the Claude Code plugin (Windows).
#
# A desktop-mcp.exe on PATH runs as is. Otherwise the release build of this
# plugin's version is downloaded once into a per-user cache, checked against
# the release's SHA256SUMS and run from there: an archive that does not match
# is deleted, never run. stdout carries the MCP stream, so every message goes
# to stderr. Not run on real Windows yet.
#
# DESKTOP_MCP_RELEASES_URL replaces the GitHub releases URL (tests).
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Kept equal to rust/Cargo.toml by rust/tests/plugin_package.rs.
$version = '0.18.0'
$releases = $env:DESKTOP_MCP_RELEASES_URL
if (-not $releases) { $releases = 'https://github.com/ahmetadakli/kde-desktop-mcp/releases' }

function Say([string]$message) {
    [Console]::Error.WriteLine("desktop-mcp plugin: $message")
}

function Install-Release([string]$root, [string]$dir, [string]$exe) {
    $name = "desktop-mcp-v$version-x86_64-windows"
    $url = "$releases/download/v$version"
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $tmp = Join-Path $root ('.download.' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
        [Net.ServicePointManager]::SecurityProtocol = `
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $zip = Join-Path $tmp "$name.zip"
        $sums = Join-Path $tmp 'SHA256SUMS'
        Say "downloading $name.zip"
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$url/$name.zip" -OutFile $zip
            Invoke-WebRequest -UseBasicParsing -Uri "$url/SHA256SUMS" -OutFile $sums
        } catch {
            # A version bump reaches the plugin before its release is published.
            throw "could not download release v$version (it may not be published yet): $_"
        }
        $want = $null
        foreach ($line in Get-Content -LiteralPath $sums) {
            $fields = $line.Trim() -split '\s+', 2
            if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -eq "$name.zip") {
                $want = $fields[0].ToLowerInvariant()
                break
            }
        }
        $have = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if (-not $want -or $want -ne $have) {
            throw "checksum mismatch for $name.zip (SHA256SUMS: $want, file: $have); not run"
        }
        Expand-Archive -LiteralPath $zip -DestinationPath $tmp
        $new = Join-Path (Join-Path $tmp $name) 'desktop-mcp.exe'
        if (-not (Test-Path -LiteralPath $new -PathType Leaf)) {
            throw "$name.zip has no desktop-mcp.exe"
        }
        # One rename: a second server starting at the same time never runs a
        # half-written file. A launcher that got there first may already run
        # its copy, which Windows will not replace: that copy is the same.
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        try {
            Move-Item -Force -LiteralPath $new -Destination $exe
        } catch {
            if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw }
        }
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
    }
}

try {
    $exe = Get-Command -Name 'desktop-mcp.exe' -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1 -ExpandProperty Source
    if (-not $exe) {
        # Claude Code passes CLAUDE_PLUGIN_DATA: kept across plugin updates,
        # removed with the plugin.
        $root = $env:CLAUDE_PLUGIN_DATA
        if (-not $root) { $root = Join-Path $env:LOCALAPPDATA 'desktop-mcp\plugin' }
        $dir = Join-Path $root "v$version"
        $exe = Join-Path $dir 'desktop-mcp.exe'
        if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
            Install-Release $root $dir $exe
        }
    }
} catch {
    Say $_
    exit 1
}

# Run last in the script and not piped, the server gets this process's stdin
# and stdout directly, which are Claude Code's pipes. Windows PowerShell 5.1
# turns each line the server writes to a redirected stderr into an error
# record: under 'Stop' the first one would end the script and the server.
$ErrorActionPreference = 'Continue'
& $exe @args
exit $LASTEXITCODE
