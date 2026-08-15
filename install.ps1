# One-line: irm https://github.com/jason920612/grokaagent/releases/latest/download/install.ps1 | iex
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = if ($env:GROKA_UPDATE_REPO) { $env:GROKA_UPDATE_REPO } else { "jason920612/grokaagent" }
$InstallDir = if ($env:GROKA_INSTALL_DIR) { $env:GROKA_INSTALL_DIR } else {
    Join-Path $env:USERPROFILE ".grokaagent\bin"
}

$osArch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $osArch = $env:PROCESSOR_ARCHITEW6432 }
if ($osArch -ne "AMD64") {
    throw "unsupported Windows arch '$osArch' (need amd64)"
}

$Asset = "grokaagent-x86_64-pc-windows-msvc.exe"
$Base = "https://github.com/$Repo/releases/latest/download"
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("grokaagent-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "downloading grokaagent ($Asset)..."
    Invoke-WebRequest -Uri "$Base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS") -UseBasicParsing
    Invoke-WebRequest -Uri "$Base/$Asset" -OutFile (Join-Path $tmp $Asset) -UseBasicParsing

    $expected = $null
    Get-Content -LiteralPath (Join-Path $tmp "SHA256SUMS") | ForEach-Object {
        if ($_ -match '^(?i)([0-9a-f]{64})\s+\*?(\S+)$' -and $Matches[2] -eq $Asset) {
            $expected = $Matches[1].ToLowerInvariant()
        }
    }
    if (-not $expected) {
        throw "SHA256SUMS has no entry for $Asset"
    }

    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $tmp $Asset)).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "checksum mismatch for $Asset`n  expected $expected`n  got      $actual"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $dest = Join-Path $InstallDir "grokaagent.exe"
    Copy-Item -LiteralPath (Join-Path $tmp $Asset) -Destination $dest -Force

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $userPath) { $userPath = "" }
    $parts = $userPath.Split(";", [System.StringSplitOptions]::RemoveEmptyEntries)
    if (-not ($parts -contains $InstallDir)) {
        $joined = if ($userPath.Trim().Length -eq 0) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $joined, "User")
        Write-Host "added $InstallDir to your user PATH"
    }
    if (-not (($env:Path -split ";") -contains $InstallDir)) {
        $env:Path = "$InstallDir;$env:Path"
    }

    Write-Host "installed $dest"
    Write-Host "open a new terminal in a project directory, then run: grokaagent"
    Write-Host "later: grokaagent update   (TUI also auto-updates from GitHub Releases)"
}
finally {
    Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
