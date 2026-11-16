# enishi installer for Windows (x86_64). In PowerShell (no admin needed):
#   irm https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.ps1 | iex
# From cmd:  powershell -NoProfile -c "irm https://raw.githubusercontent.com/rauldev-yoru/enishi/main/install.ps1 | iex"
# Env: $env:ENISHI_VERSION = 'v0.1.0' to pin a release (default: newest, pre-releases included).
#      $env:ENISHI_INSTALL_ARGS = '--firewall' to pass extra args to `enishi install`.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

    $repo  = 'rauldev-yoru/enishi'
    $asset = 'enishi-x86_64-pc-windows-msvc.exe'

    if (-not [Environment]::Is64BitOperatingSystem) { throw 'enishi: only x86_64 Windows is supported for now' }

    $tag = $env:ENISHI_VERSION
    if (-not $tag) {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases?per_page=1" -Headers @{ 'User-Agent' = 'enishi-install' }
        $tag = @($rel)[0].tag_name
        if (-not $tag) { throw "enishi: could not find a release of $repo" }
    }
    $base = "https://github.com/$repo/releases/download/$tag"
    Write-Host "enishi-install: installing enishi $tag"

    $dir = Join-Path $env:TEMP ("enishi-install-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $exe  = Join-Path $dir 'enishi-setup.exe'
        $sums = Join-Path $dir 'SHA256SUMS'
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $exe
        Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums
        $line = Get-Content $sums | Where-Object { $_ -match " \*?$([regex]::Escape($asset))$" } | Select-Object -First 1
        if (-not $line) { throw "enishi: $asset not listed in SHA256SUMS" }
        $want = ($line -split '\s+')[0].ToLower()
        $got  = (Get-FileHash -Algorithm SHA256 $exe).Hash.ToLower()
        if ($want -ne $got) { throw "enishi: checksum mismatch for $asset" }
        Write-Host 'enishi-install: checksum ok'
        $extra = @()
        if ($env:ENISHI_INSTALL_ARGS) { $extra = $env:ENISHI_INSTALL_ARGS -split '\s+' }
        & $exe install @extra
        if ($LASTEXITCODE -ne 0) { throw "enishi install failed (exit $LASTEXITCODE)" }
    } finally {
        Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
    }
}
