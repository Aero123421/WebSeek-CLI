# webseek installer for Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/Aero123421/WebSeek-CLI/main/install.ps1 | iex
#
# Environment overrides:
#   WEBSEEK_INSTALL_DIR  where to place webseek.exe
#                        (default: %LOCALAPPDATA%\webseek\bin)
#   WEBSEEK_TAG          release tag to install (default: latest)
#
# The script downloads the release zip and SHA256SUMS.txt, verifies the
# checksum, installs webseek.exe, and adds the install dir to your user PATH.

$ErrorActionPreference = 'Stop'

$Repo = 'Aero123421/WebSeek-CLI'
$Api = "https://api.github.com/repos/$Repo/releases"
$Target = 'x86_64-pc-windows-msvc'

# --- Resolve release tag -----------------------------------------------------
if ($env:WEBSEEK_TAG) {
    $Tag = $env:WEBSEEK_TAG
}
else {
    $latest = Invoke-RestMethod -Uri "$Api/latest" -Headers @{ 'User-Agent' = 'webseek-installer' }
    $Tag = $latest.tag_name
    if (-not $Tag) { throw 'could not determine the latest release tag (API unreachable?)' }
}
Write-Host "Installing webseek $Tag ($Target)"

$Asset = "webseek-$Tag-$Target.zip"
$Base = "https://github.com/$Repo/releases/download/$Tag"

# --- Download ----------------------------------------------------------------
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) "webseek-install-$([System.Guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Zip = Join-Path $Tmp $Asset
    $Sums = Join-Path $Tmp 'SHA256SUMS.txt'
    Write-Host "Downloading $Asset"
    Invoke-WebRequest -Uri "$Base/$Asset" -OutFile $Zip -UseBasicParsing
    Invoke-WebRequest -Uri "$Base/SHA256SUMS.txt" -OutFile $Sums -UseBasicParsing

    # --- Verify checksum -----------------------------------------------------
    $Expected = (Select-String -Path $Sums -Pattern ([regex]::Escape($Asset)) |
            ForEach-Object { ($_.Line -split '\s+')[0] }) | Select-Object -First 1
    if (-not $Expected) { throw "no checksum for $Asset in SHA256SUMS.txt" }
    $Actual = (Get-FileHash -Path $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Expected -ne $Actual) { throw "checksum mismatch for $Asset (expected $Expected, got $Actual)" }
    Write-Host 'Checksum OK'

    # --- Extract -------------------------------------------------------------
    Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
    $Binary = Join-Path $Tmp 'webseek.exe'
    if (-not (Test-Path $Binary)) { throw "archive did not contain 'webseek.exe'" }

    # --- Install ---------------------------------------------------------------
    $InstallDir = if ($env:WEBSEEK_INSTALL_DIR) { $env:WEBSEEK_INSTALL_DIR }
    else { Join-Path $env:LOCALAPPDATA 'webseek\bin' }
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $Destination = Join-Path $InstallDir 'webseek.exe'
    Move-Item -Path $Binary -Destination $Destination -Force

    # --- Add to user PATH if missing -------------------------------------------
    function Get-NormalizedPath([string]$Path) {
        try { [System.IO.Path]::GetFullPath($Path).TrimEnd('\', '/') }
        catch { $Path.TrimEnd('\', '/') }
    }
    $CurrentPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $Dirs = @($CurrentPath -split ';' | Where-Object { $_ })
    $NormalizedInstallDir = Get-NormalizedPath $InstallDir
    $NormalizedDirs = @($Dirs | ForEach-Object { Get-NormalizedPath $_ })
    if ($NormalizedDirs -notcontains $NormalizedInstallDir) {
        [Environment]::SetEnvironmentVariable('Path', (($Dirs + $InstallDir) -join ';'), 'User')
        Write-Host "Added $InstallDir to your user PATH (restart your shell to use it)"
    }

    Write-Host "Installed webseek $Tag to $Destination"
    Write-Host 'Try it: webseek search "rust async runtime"'
}
finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}
