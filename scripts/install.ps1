#requires -Version 5.1
<#
.SYNOPSIS
    Installs a released g-mesh on Windows: downloads the x86_64-pc-windows-msvc
    release archive from GitHub Releases, verifies its SHA-256 before unpacking,
    and puts core *and* its bundled plugins on disk together.

.DESCRIPTION
    This is install.sh's Windows counterpart, not a different design. install.sh
    (scripts/install.sh) refuses on Windows on purpose - a POSIX shell has no
    portable way to unpack a .zip - and prints three manual steps instead. This
    script does those three steps, in PowerShell, so a Windows user gets the
    same one-line install everyone else does. Every decision below is inherited
    from install.sh for the reason given there; only the platform-specific parts
    (archive format, executable name, PATH mechanics) differ.

    Usage:
        irm https://raw.githubusercontent.com/madmurdok/g-mesh/main/scripts/install.ps1 | iex
        irm .../install.ps1 | iex; Install-GMesh -Version 2.7.0
        pwsh scripts/install.ps1 -InstallDir C:\opt\g-mesh   # from a checkout

    Environment (same names install.sh uses, so a mixed macOS/Linux/Windows
    fleet can be driven by one set of variables):
        G_MESH_VERSION        version to install (default: latest published)
        G_MESH_INSTALL_DIR    where to install (default: $env:USERPROFILE\.g-mesh\bin)
        G_MESH_TARGET         override the detected target triple (advanced/testing)
        G_MESH_REPO           owner/repo (default: madmurdok/g-mesh)
        G_MESH_DOWNLOAD_BASE  base for <version-tag>/<asset> URLs
        G_MESH_LATEST_API     the releases/latest endpoint
        GITHUB_TOKEN          if set, authenticates the API call (rate limits)

.PARAMETER Version
    Install this release instead of the latest published one, e.g. 2.7.0.

.PARAMETER InstallDir
    Install root (default: $env:USERPROFILE\.g-mesh\bin). The binary and its
    plugins\ directory both live here, and this is the directory that goes on
    PATH.

.PARAMETER Target
    Override platform detection (advanced/testing). The only target this
    script installs is x86_64-pc-windows-msvc; anything else is refused the
    same way install.sh refuses a target it does not publish.

.PARAMETER Force
    Replace a non-empty install directory that does not look like an existing
    g-mesh install.
#>
[CmdletBinding()]
param(
    [string]$Version = $(if ($env:G_MESH_VERSION) { $env:G_MESH_VERSION } else { '' }),
    [string]$InstallDir = $(if ($env:G_MESH_INSTALL_DIR) { $env:G_MESH_INSTALL_DIR } else { '' }),
    [string]$Target = $(if ($env:G_MESH_TARGET) { $env:G_MESH_TARGET } else { '' }),
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# WHAT GETS INSTALLED, AND WHY IT IS A DIRECTORY AND NOT ONE FILE
#
# Same reason as install.sh: a release archive is a complete install, not a
# binary. g-mesh.exe, plugins\typescript\, plugins\rust\, plugins\python\,
# LICENSE*, README.md all travel together, because core finds its plugins by
# looking for `plugins\` *next to the executable that is running*
# (`daemon::manifest::installed_bundled_root`, which is
# `std::env::current_exe()` + `\plugins`).
#
# Windows adds its own trap on top of install.sh's symlink one: a shortcut
# (.lnk) is not a symlink and is never resolved by `current_exe()` - it is a
# separate file the shell interprets, invisible to the process it launches -
# and neither is a `doskey` alias, which is a text substitution the console
# host performs before the process ever starts and carries no path
# information into it at all. "Just put g-mesh.exe on PATH via a shortcut/
# alias" looks like it should work and instead reproduces install.sh's macOS
# symlink failure: the running process resolves back to wherever the real
# .exe sits, looks for `plugins\` beside *that*, and if the shortcut/alias
# points somewhere other than the real install directory, finds nothing. That
# failure is silent and specific - it does not refuse to start, it starts and
# then cannot index anything, which looks exactly like "no plugins
# installed" until someone thinks to check the executable's actual path. So,
# like install.sh, this script creates no shortcut and no alias; it prints
# the one PATH line to add and touches no profile of yours.
#
# Default location: $env:USERPROFILE\.g-mesh\bin - the same layout choice
# install.sh makes for the same reason: it is inside the directory g-mesh
# already owns (config, project indexes, the embedding model all live under
# $env:USERPROFILE\.g-mesh), so uninstalling is `Remove-Item -Recurse -Force
# $env:USERPROFILE\.g-mesh\bin` and settings survive it.
#
# ---------------------------------------------------------------------------
# CHECKSUMS
#
# Identical policy to install.sh, because the release publishes the checksum
# the same way regardless of which platform is installing: the per-asset
# <archive>.sha256 file is fetched, hashed locally with
# Get-FileHash -Algorithm SHA256 (built into PowerShell 5.1+, no external tool
# needed - Windows has no sha256sum/shasum/openssl guarantee the way
# install.sh's POSIX targets do), and compared BEFORE a single byte is
# unpacked. A mismatch aborts with both hashes printed and nothing installed.
# There is no flag to skip this.
#
# ---------------------------------------------------------------------------
# VERSIONS, AND THE "NOTHING IS PUBLISHED YET" CASE
#
# Same reasoning as install.sh: asset names embed the version
# (g-mesh-v<version>-x86_64-pc-windows-msvc.zip), so `/releases/latest/
# download/...` cannot be used - the file cannot be named without first
# knowing the version. The version comes from the releases/latest API, or
# from -Version/$env:G_MESH_VERSION to pin one. Releases are created as
# DRAFTS (.github/workflows/release.yml) and stay invisible - their asset
# URLs 404 - until a human publishes them, so "no published release" gets the
# same explicit message install.sh gives, not a bare 404.
#
# ---------------------------------------------------------------------------
# TRANSPORT
#
# install.sh pins `--proto '=https' --tlsv1.2` on curl, but only when the URL
# is already https - so a local http:// fixture server still works for
# testing. Invoke-WebRequest has no equivalent pinning flag on Windows
# PowerShell 5.1 (`-SslProtocol` was only added in PowerShell 6), and .NET's
# HTTP stack on any Windows new enough to matter already negotiates TLS 1.2+
# by default without being told to, so there is nothing to pin here - and,
# same as install.sh, nothing to change for an http:// G_MESH_DOWNLOAD_BASE
# either, since no protocol-specific branch exists to skip.
#
# ---------------------------------------------------------------------------
# TESTING IT WITHOUT A RELEASE
#
# Same technique install.sh's own header describes: every URL is injectable,
# so this was exercised against a local fixture (a real archive built by
# scripts/build-targets.sh's Windows job, served over 127.0.0.1) before any
# Windows release existed:
#
#   $env:G_MESH_DOWNLOAD_BASE = 'http://127.0.0.1:8000'
#   $env:G_MESH_INSTALL_DIR   = 'C:\Temp\g-mesh-test'
#   pwsh scripts/install.ps1 -Version 2.7.0
#
# ---------------------------------------------------------------------------
# WHAT THIS SCRIPT DOES NOT PROVE
#
# This installs the Windows artifact; it does not prove the Windows artifact
# is correct. It downloads, verifies the checksum, unpacks, runs the binary
# once (the same smoke test install.sh does - see Install-GMesh below), and
# advises on PATH. None of that depends on what plugins\typescript\ contains
# internally - whether it is a Node SEA or, later, a native binary - only on
# the archive's shape, which install.sh already establishes and this script
# inherits unchanged. Proof that the artifact this script installs actually
# works end to end on a real Windows machine is GM-333's job (a release
# workflow step that runs each artifact on its own runner), not this
# script's.
# ---------------------------------------------------------------------------

$Repo = if ($env:G_MESH_REPO) { $env:G_MESH_REPO } else { 'madmurdok/g-mesh' }
$DownloadBase = if ($env:G_MESH_DOWNLOAD_BASE) { $env:G_MESH_DOWNLOAD_BASE } else { "https://github.com/$Repo/releases/download" }
$LatestApi = if ($env:G_MESH_LATEST_API) { $env:G_MESH_LATEST_API } else { "https://api.github.com/repos/$Repo/releases/latest" }
if (-not $InstallDir) {
    $InstallDir = Join-Path $env:USERPROFILE '.g-mesh\bin'
}

# The only Windows target the release pipeline publishes
# (.github/workflows/release.yml's build matrix; see scripts/build-targets.sh
# SUPPORTED_TARGETS). install.sh's SUPPORTED_TARGETS deliberately excludes
# this one and refuses to it; this script is the mirror image - it installs
# only this one and refuses to anything else.
$SupportedTarget = 'x86_64-pc-windows-msvc'

function Write-Log {
    param([string]$Message)
    Write-Host "==> $Message"
}

function Die {
    param([string]$Message)
    # `throw`, not `exit`: this file is meant to run the same way install.sh's
    # README shows `curl | sh` running - as a one-liner piped into the
    # interpreter (`irm ... | iex`) in the user's own interactive session.
    # `curl | sh`'s `exit 1` only ever kills the `sh` subprocess that pipe
    # spawned; the caller's real shell survives untouched. `iex` has no such
    # subprocess boundary - it evaluates in the *current* session - so a bare
    # `exit` here would close the user's whole PowerShell window on every
    # handled failure (a bad --version, a checksum mismatch, an unpublished
    # release), which is strictly worse than what it mirrors. A thrown
    # terminating error instead unwinds through every open `finally` (so
    # Install-GMesh's temp-directory cleanup still runs - see its own
    # `finally`, the equivalent of install.sh's `trap ... EXIT`), prints as an
    # error at the console without ending the session, and still leaves a
    # non-zero exit code behind for a non-interactive caller
    # (`pwsh -File install.ps1`, or `-Command "irm ... | iex"` in a CI step).
    throw "install: $Message"
}

# ---------------------------------------------------------------------------
# Platform detection
#
# install.sh asks `uname`; there is no uname here, so the equivalent question
# - "is this even the platform this script targets, and if so which arch" -
# is asked through .NET/PowerShell's own runtime info. Like install.sh's
# Rosetta check (it asks the kernel, not just uname -m, because a translated
# shell misreports its own arch), this asks the OS for the *process*
# architecture rather than trusting $env:PROCESSOR_ARCHITECTURE alone, since
# that variable reports the architecture of the shell running the script, not
# necessarily the machine's - a 32-bit PowerShell on 64-bit Windows would
# misreport the same way a translated shell does on macOS.
function Get-Target {
    # $IsWindows only exists as an automatic variable on PowerShell 6+
    # (pwsh is cross-platform); Windows PowerShell 5.1 has no such variable
    # because it only ever runs on Windows. Under Set-StrictMode -Version
    # Latest, referencing $IsWindows directly on 5.1 would itself be a
    # fatal "variable not set" error, so its presence is checked with
    # Get-Variable first rather than assumed.
    $isWindowsVar = Get-Variable -Name IsWindows -ErrorAction SilentlyContinue
    if ($isWindowsVar -and -not $isWindowsVar.Value) {
        Die "this script only installs g-mesh on Windows (target $SupportedTarget). On macOS or Linux, use scripts/install.sh instead."
    }

    $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    switch ($arch) {
        'X64' { return $SupportedTarget }
        'Arm64' {
            Die "no aarch64 Windows build is published - g-mesh releases cover $SupportedTarget only. Build from source: https://github.com/$Repo#build"
        }
        default {
            Die "unsupported Windows architecture: $arch - g-mesh releases cover $SupportedTarget only. Build from source: https://github.com/$Repo#build"
        }
    }
}

# ---------------------------------------------------------------------------
# Version resolution
#
# Mirrors install.sh's resolve_latest_version/no_published_release exactly:
# the API call either answers with a tag or it does not, so this cannot claim
# to know *why* it failed, and lists the causes in the same order install.sh
# does - rate limit first (most likely once a release actually exists), no
# network route second, "genuinely no release yet" third.
function Get-NoPublishedReleaseMessage {
    @"
install: could not work out which release to install.

This asked for the latest published release and got no answer:

  $LatestApi

Most likely, in this order:
  - The GitHub API rate-limited you. Unauthenticated calls get 60 per hour
    per IP, and this looks identical to "no release exists". Set GITHUB_TOKEN
    to raise the limit, or wait an hour.
  - You have no route to api.github.com - a proxy, a firewall, or no network.
  - There genuinely is no published release. Releases here are built as
    drafts and stay invisible, with their download URLs 404ing, until a
    human publishes one, so this is the expected state between a build
    finishing and someone pressing Publish.

What you can do:
  - Check https://github.com/$Repo/releases to see what is published.
  - Install a specific version, skipping the API call entirely:
      irm https://raw.githubusercontent.com/$Repo/main/scripts/install.ps1 | iex; Install-GMesh -Version X.Y.Z
  - Build from source meanwhile: https://github.com/$Repo#build
"@
}

function Resolve-LatestVersion {
    $headers = @{ Accept = 'application/vnd.github+json' }
    if ($env:GITHUB_TOKEN) {
        $headers['Authorization'] = "Bearer $env:GITHUB_TOKEN"
    }
    try {
        $release = Invoke-RestMethod -Uri $LatestApi -Headers $headers -ErrorAction Stop
    }
    catch {
        Die (Get-NoPublishedReleaseMessage)
    }
    $tag = $release.tag_name
    if (-not $tag) {
        Die (Get-NoPublishedReleaseMessage)
    }
    return $tag.TrimStart('v')
}

# ---------------------------------------------------------------------------
# Checksums
#
# Get-FileHash -Algorithm SHA256 is built into PowerShell 5.1+ (backed by
# .NET's SHA256 implementation), so unlike install.sh's sha256sum/shasum/
# openssl fallback chain there is exactly one way to do this and it always
# exists - no "none of these tools are on this machine" case to handle.
# Compared lowercase, same as install.sh, since the .sha256 file's casing is
# not guaranteed and PowerShell's Hash property is uppercase hex.
function Get-Sha256 {
    param([string]$Path)
    (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# Windows will not do what install.sh's `mv "$_stage" "$_new"` does for free.
# POSIX mv falls back to copy-then-unlink when source and destination are on
# different filesystems, so install.sh's stage step never has to think about
# it. .NET's Directory.Move (what Move-Item uses under the hood) refuses
# outright with "Source and destination path must have identical roots" when
# the two sides are on different drives - and $work lives under
# $env:TEMP while $InstallDir (and therefore its ".new-<pid>" sibling) can be
# anywhere the caller names with -InstallDir, so this is a real case, not a
# theoretical one, for anyone who points -InstallDir at a different drive
# than %TEMP%. The two same-directory renames further down ($InstallDir <->
# its .old-<pid>/.new-<pid> siblings) can never cross drives, since a sibling
# path is by construction on the same volume as $InstallDir; only staging the
# freshly-unpacked archive out of $work needs this fallback.
function Move-DirectoryRobust {
    param([string]$Source, [string]$Destination)
    try {
        Move-Item -LiteralPath $Source -Destination $Destination -ErrorAction Stop
    }
    catch {
        # Same-volume moves never reach here; a cross-volume one falls back
        # to copy + delete, which is what POSIX mv itself does in that case
        # too - non-atomic either way once two filesystems are involved.
        Copy-Item -LiteralPath $Source -Destination $Destination -Recurse -ErrorAction Stop
        Remove-Item -LiteralPath $Source -Recurse -Force
    }
}

# Installing replaces the whole install directory - same policy and same
# reasoning as install.sh's check_install_dir: an old install's stale plugin
# files should not linger beside new ones, a mistyped -InstallDir should not
# silently eat an unrelated directory, and the check runs before the download
# so a typo costs a moment, not tens of megabytes.
# -Force is a parameter here, passed explicitly by Install-GMesh (GM-347).
# It used to be read without being declared, resolving to Install-GMesh's
# $Force through the caller's scope - under Set-StrictMode that throws when
# called from any scope without one, turning a refusal into a crash.
function Test-InstallDir {
    param(
        [string]$Dir,
        [switch]$Force
    )
    if (-not (Test-Path -LiteralPath $Dir)) {
        return
    }
    if (-not (Test-Path -LiteralPath $Dir -PathType Container)) {
        Die "$Dir exists and is not a directory"
    }
    if (Test-Path -LiteralPath (Join-Path $Dir 'g-mesh.exe')) {
        return
    }
    $existing = Get-ChildItem -LiteralPath $Dir -Force -ErrorAction SilentlyContinue
    if (-not $existing) {
        return
    }
    if (-not $Force) {
        Die "$Dir is not empty and does not look like a g-mesh install (no g-mesh.exe in it). Installing would replace its whole contents - pass a different -InstallDir, or -Force if you meant this one."
    }
}

function Install-GMesh {
    [CmdletBinding()]
    param(
        [string]$Version = '',
        [string]$InstallDir = '',
        [string]$Target = '',
        [switch]$Force
    )

    if (-not $InstallDir) {
        Die "no install directory: pass -InstallDir, or set `$env:USERPROFILE"
    }
    if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
        $InstallDir = Join-Path (Get-Location) $InstallDir
    }

    Test-InstallDir -Dir $InstallDir -Force:$Force

    if (-not $Target) {
        $Target = Get-Target
    }
    elseif ($Target -ne $SupportedTarget) {
        Die "unsupported target: $Target - this script only installs $SupportedTarget. Use scripts/install.sh on macOS/Linux for the other targets."
    }

    if ($Version) {
        $Version = $Version.TrimStart('v')
        if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
            Die "malformed version: '$Version' (expected X.Y.Z, e.g. 2.7.0)"
        }
    }
    else {
        Write-Log "resolving the latest published release"
        $Version = Resolve-LatestVersion
    }

    $stem = "g-mesh-v$Version-$Target"
    $asset = "$stem.zip"
    $url = "$DownloadBase/v$Version/$asset"

    Write-Log "g-mesh $Version for $Target -> $InstallDir"

    $work = Join-Path ([System.IO.Path]::GetTempPath()) "g-mesh-install-$([System.IO.Path]::GetRandomFileName())"
    New-Item -ItemType Directory -Path $work -Force | Out-Null
    try {
        $archivePath = Join-Path $work $asset
        $shaPath = "$archivePath.sha256"

        Write-Log "downloading $asset"
        try {
            Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing -ErrorAction Stop
        }
        catch {
            Die "could not download $url`nThe release may not be published yet, or may not include a build for $Target.`nCheck https://github.com/$Repo/releases"
        }

        Write-Log "downloading its checksum"
        try {
            Invoke-WebRequest -Uri "$url.sha256" -OutFile $shaPath -UseBasicParsing -ErrorAction Stop
        }
        catch {
            Die "could not download $url.sha256`nThe archive downloaded but its checksum did not, so the download cannot be`nverified - refusing to install unverified bytes."
        }

        # The .sha256 file is `<hex>  <basename>`, written by build-targets.sh
        # and re-checked at publish time by prepare-release-assets.sh; only
        # the digest matters here, since we know which file we just fetched.
        $shaLine = Get-Content -LiteralPath $shaPath -TotalCount 1
        $expected = if ($shaLine) { ($shaLine -split '\s+')[0].ToLowerInvariant() } else { '' }
        if (-not $expected) {
            Die "the published checksum file for $asset is empty or malformed - refusing to install unverified bytes"
        }
        $actual = Get-Sha256 -Path $archivePath
        if ($expected -ne $actual) {
            Die "checksum mismatch for $asset - NOTHING was installed.`n  expected: $expected`n  actual:   $actual`nThe download is corrupt or has been tampered with. Retry; if it keeps`nfailing, report it at https://github.com/$Repo/issues rather than installing`nthis binary."
        }
        Write-Log "checksum ok"

        Write-Log "unpacking"
        $unpackDir = Join-Path $work 'unpack'
        New-Item -ItemType Directory -Path $unpackDir -Force | Out-Null
        try {
            Expand-Archive -LiteralPath $archivePath -DestinationPath $unpackDir -Force
        }
        catch {
            Die "could not unpack $asset (checksum matched, so this is an unzip problem, not a corrupt download)"
        }

        $stage = Join-Path $unpackDir $stem
        if (-not (Test-Path -LiteralPath $stage -PathType Container)) {
            Die "unexpected archive layout: $asset does not contain a $stem\ directory"
        }
        $exePath = Join-Path $stage 'g-mesh.exe'
        if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) {
            Die "unexpected archive layout: no g-mesh.exe inside $asset"
        }
        $tsPluginToml = Join-Path $stage 'plugins\typescript\plugin.toml'
        if (-not (Test-Path -LiteralPath $tsPluginToml -PathType Leaf)) {
            Die "unexpected archive layout: $asset carries no plugins\typescript\plugin.toml. Core cannot index a TypeScript project without it, so this archive is not installable."
        }

        # Run it before installing it, same as install.sh: '--version' proves
        # the binary executes on this machine at all, and 'plugins list'
        # proves it discovers the plugin that travelled with it - the one
        # failure mode that otherwise shows up only later, as a daemon that
        # refuses to start. This is a per-machine check, not a claim about
        # the artifact in general: it can only prove the copy just
        # downloaded runs *here*, on this Windows machine, right now.
        Write-Log "verifying the downloaded binary runs"
        try {
            $reportedLines = & $exePath --version 2>$null
        }
        catch {
            $reportedLines = $null
        }
        # & returns one string per output line (System.Object[] when there is
        # more than one); joined here so -match/-notmatch test the whole
        # output as one string instead of PowerShell's array-match semantics
        # (matching element-by-element and returning the surviving elements),
        # which is not what "does the version appear anywhere in the output"
        # means.
        $reported = if ($reportedLines) { $reportedLines -join "`n" } else { '' }
        if (-not $reported -or $LASTEXITCODE -ne 0) {
            Die "the downloaded g-mesh does not run on this machine (target $Target) - nothing was installed"
        }
        if ($reported -notmatch [regex]::Escape($Version)) {
            Die "version mismatch: the archive is named $Version but the binary reports '$reported' - nothing was installed"
        }
        $pluginsLines = & $exePath plugins list 2>$null
        $pluginsOutput = if ($pluginsLines) { $pluginsLines -join "`n" } else { '' }
        if ($pluginsOutput -notmatch 'typescript') {
            Die "the downloaded g-mesh does not see the plugin that shipped with it - nothing was installed"
        }

        Write-Log "installing into $InstallDir"
        $parent = Split-Path -Parent $InstallDir
        if ($parent -and -not (Test-Path -LiteralPath $parent)) {
            New-Item -ItemType Directory -Path $parent -Force | Out-Null
        }
        $suffix = [System.Diagnostics.Process]::GetCurrentProcess().Id
        $new = "$InstallDir.new-$suffix"
        $old = "$InstallDir.old-$suffix"
        Remove-Item -LiteralPath $new -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $old -Recurse -Force -ErrorAction SilentlyContinue
        try {
            Move-DirectoryRobust -Source $stage -Destination $new
        }
        catch {
            Die "could not stage the new install at $new"
        }
        if (Test-Path -LiteralPath $InstallDir) {
            try {
                Move-Item -LiteralPath $InstallDir -Destination $old
            }
            catch {
                Remove-Item -LiteralPath $new -Recurse -Force -ErrorAction SilentlyContinue
                Die "could not move the existing install aside ($InstallDir) - nothing was changed"
            }
        }
        try {
            Move-Item -LiteralPath $new -Destination $InstallDir
        }
        catch {
            # Put the previous install back rather than leaving the machine
            # with neither.
            if (Test-Path -LiteralPath $old) {
                Move-Item -LiteralPath $old -Destination $InstallDir -Force
            }
            Remove-Item -LiteralPath $new -Recurse -Force -ErrorAction SilentlyContinue
            Die "could not install into $InstallDir (permissions?) - the previous install was left in place"
        }
        Remove-Item -LiteralPath $old -Recurse -Force -ErrorAction SilentlyContinue

        Write-Host ""
        Write-Log "installed g-mesh $Version"
        Write-Host "  binary:  $InstallDir\g-mesh.exe"
        Write-Host "  plugins: $InstallDir\plugins\  (must stay beside the binary)"
        Write-Host ""

        $pathEntries = $env:Path -split ';'
        if ($pathEntries -contains $InstallDir) {
            Write-Host "$InstallDir is already on your PATH. Try:"
            Write-Host ""
            Write-Host "  g-mesh --version"
        }
        else {
            # Unlike install.sh (which never edits a shell rc file because
            # POSIX shells don't agree on one, and printing an export line is
            # the portable answer), Windows has a single per-user PATH stored
            # in the registry that every shell reads, and
            # [Environment]::SetEnvironmentVariable is the standard,
            # non-destructive way to append to it - it does not touch any
            # shell profile file and takes effect in new sessions
            # immediately, without a logout/logon. It still does not touch
            # the *current* process's $env:Path, so this session needs the
            # explicit line below too, same as install.sh's advice needs a
            # `source`/new shell.
            Write-Host "Add it to your PATH - this script does not edit shell profile files:"
            Write-Host ""
            Write-Host "  [Environment]::SetEnvironmentVariable('Path', `$env:Path + ';$InstallDir', 'User')"
            Write-Host ""
            Write-Host "Then, in a new terminal: g-mesh --version"
            Write-Host "(Or, for this session only: `$env:Path += ';$InstallDir')"
        }
        Write-Host ""
        Write-Host "Register it with Claude Code:"
        Write-Host ""
        Write-Host "  claude mcp add g-mesh -s user -- $InstallDir\g-mesh.exe mcp-shim"
        Write-Host ""
        Write-Host "The seven structural tools work as-is. ``search_code`` additionally needs"
        Write-Host "the embedding model: g-mesh model fetch"
        Write-Host ""
        Write-Host "To uninstall: Remove-Item -Recurse -Force $InstallDir"
    }
    finally {
        Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Piped through `irm ... | iex`, this file only *defines* Install-GMesh (a
# function, not a script invoked with args, since iex has no argv to hand
# it) - so it must also call it, the same way install.sh's own `main "$@"` at
# the bottom runs unconditionally whether the file was executed directly or
# fetched and piped into `sh`. Running as `pwsh scripts/install.ps1 -Version
# X` (a checkout, not a pipe) reaches here through the same path, since
# $Version/$InstallDir/$Target/$Force were already bound from the script's
# own param() block above and Install-GMesh's defaults just forward them.
Install-GMesh -Version $Version -InstallDir $InstallDir -Target $Target -Force:$Force
