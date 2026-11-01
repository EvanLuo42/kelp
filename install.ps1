<#
.SYNOPSIS
Installs Kelp from a GitHub Release (Windows x86_64).

.DESCRIPTION
  irm https://raw.githubusercontent.com/EvanLuo42/kelp/main/install.ps1 | iex

Through `iex` no arguments can be passed, so every option also has an
environment variable. To pass arguments, download the script and run it.

.PARAMETER Components
What to install: all, cli (kelp and kelpd), server (kelp-server) or "cli,server".
Asked when omitted in a terminal, otherwise all. Environment: KELP_COMPONENTS.

.PARAMETER InstallDir
Installation directory. Asked when omitted in a terminal, otherwise
%USERPROFILE%\.kelp\bin. Environment: KELP_INSTALL_DIR.

.PARAMETER Version
Release to install, such as v0.1.1. Default: latest. Environment: KELP_VERSION.

.PARAMETER NoModifyPath
Do not add the directory to the user PATH. Environment: KELP_NO_MODIFY_PATH=1.

.PARAMETER Yes
Do not ask questions; use defaults for the rest. Environment: KELP_YES=1.
#>
param(
    [string]$Components = $env:KELP_COMPONENTS,
    [string]$InstallDir = $env:KELP_INSTALL_DIR,
    [string]$Version = $env:KELP_VERSION,
    [switch]$NoModifyPath = [bool]$env:KELP_NO_MODIFY_PATH,
    [switch]$Yes = [bool]$env:KELP_YES
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'EvanLuo42/kelp'
$target = 'x86_64-pc-windows-msvc'

if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    throw "No prebuilt binaries for this architecture; use 'cargo install kelp-server kelp-cli' instead."
}

# Reads an answer; returns '' without a terminal or with -Yes.
function Ask([string]$Prompt) {
    if ($Yes) { return '' }
    try { return (Read-Host $Prompt).Trim() } catch { return '' }
}

# --- choices

if (-not $Components) {
    $answer = Ask @"
Install what?
  1) all    - kelp, kelpd and kelp-server
  2) cli    - kelp and kelpd
  3) server - kelp-server
Choice [1]
"@
    $Components = switch ($answer) {
        { $_ -in '', '1', 'all' } { 'all' }
        { $_ -in '2', 'cli' } { 'cli' }
        { $_ -in '3', 'server' } { 'server' }
        default { throw "Unknown choice '$answer'." }
    }
}

$bins = @()
foreach ($part in ($Components -split '[,\s]+' | Where-Object { $_ })) {
    switch ($part) {
        'all' { $bins = @('kelp', 'kelpd', 'kelp-server') }
        'cli' { $bins += @('kelp', 'kelpd') }
        'server' { $bins += 'kelp-server' }
        default { throw "Unknown component '$part' (use all, cli or server)." }
    }
}

if (-not $InstallDir) {
    $default = Join-Path $HOME '.kelp\bin'
    $answer = Ask "Install into [$default]"
    $InstallDir = if ($answer) { $answer } else { $default }
}
$InstallDir = [IO.Path]::GetFullPath($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir))

# --- download

if (-not $Version) { $Version = 'latest' }
if ($env:KELP_BASE_URL) {
    $base = $env:KELP_BASE_URL
} elseif ($Version -eq 'latest') {
    $base = "https://github.com/$repo/releases/latest/download"
} else {
    if ($Version -notmatch '^v') { $Version = "v$Version" }
    $base = "https://github.com/$repo/releases/download/$Version"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("kelp-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $archive = "kelp-$target.zip"
    Write-Host "Downloading $archive ($Version)"
    & curl.exe -fsSL -o "$tmp\$archive" "$base/$archive"
    if ($LASTEXITCODE -ne 0) { throw "Could not download $base/$archive (is there a release for $target?)." }

    & curl.exe -fsL -o "$tmp\SHA256SUMS" "$base/SHA256SUMS"
    if ($LASTEXITCODE -eq 0) {
        $line = Get-Content "$tmp\SHA256SUMS" | Where-Object { $_ -match "^\s*([0-9a-fA-F]{64})\s+\*?$([regex]::Escape($archive))\s*$" }
        if (-not $line) { throw "SHA256SUMS has no entry for $archive." }
        $expected = ($line -split '\s+')[0].ToLower()
        $actual = (Get-FileHash -Algorithm SHA256 "$tmp\$archive").Hash.ToLower()
        if ($expected -ne $actual) { throw "Checksum mismatch for $archive." }
    } else {
        Write-Warning 'No SHA256SUMS in this release; skipping checksum verification.'
    }

    Expand-Archive -Path "$tmp\$archive" -DestinationPath $tmp
    $src = Join-Path $tmp "kelp-$target"

    # --- install

    foreach ($bin in $bins) {
        $running = Get-Process -Name $bin -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -eq (Join-Path $InstallDir "$bin.exe") }
        if ($running) { throw "$bin is running from $InstallDir; stop it and run the installer again." }
        if (-not (Test-Path "$src\$bin.exe")) { throw "$bin.exe is not in the archive." }
    }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($bin in $bins) {
        Copy-Item -Force "$src\$bin.exe" (Join-Path $InstallDir "$bin.exe")
        Write-Host "Installed $(Join-Path $InstallDir "$bin.exe")"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

# --- PATH

$trimmed = $InstallDir.TrimEnd('\')
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @($userPath -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') })
if ($entries -notcontains $trimmed -and ($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $trimmed) {
    if ($NoModifyPath) {
        Write-Host "$InstallDir is not in PATH; add it with: setx PATH `"%PATH%;$InstallDir`""
    } else {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "Added $InstallDir to the user PATH. Open a new terminal to use Kelp."
    }
}
$env:Path = "$env:Path;$InstallDir"

foreach ($bin in $bins) {
    & (Join-Path $InstallDir "$bin.exe") --version
}
