param(
    [string]$OpenCvRoot = $env:OPENCV_DIR
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

Push-Location $projectRoot
try {
    & cargo build -p cdr-desktop --release
    if ($LASTEXITCODE -ne 0) { throw 'Desktop release build failed.' }
} finally {
    Pop-Location
}

$releaseDir = Join-Path $projectRoot 'target\release'
Write-Output "Desktop executable: $(Join-Path $releaseDir 'cdr-desktop.exe')"
