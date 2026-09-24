param(
    [string]$Inventory = (Join-Path $PSScriptRoot '..\target\test-run\inventory.json'),
    [string]$Report = (Join-Path $PSScriptRoot '..\target\test-run\raster-report.json'),
    [string]$ShapePath = '1.9',
    [double]$MaximumSegmentMm = 5.0,
    [switch]$SkipGenerate
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $SkipGenerate) {
    $projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    Push-Location $projectRoot
    try {
        $rasterBinary = Join-Path $projectRoot 'target\debug\cdr-raster.exe'
        if (Test-Path -LiteralPath $rasterBinary -PathType Leaf) {
            & $rasterBinary $Inventory $Report
        } else {
            & cargo run -q -p cdr-raster -- $Inventory $Report
        }
        if ($LASTEXITCODE -ne 0) { throw 'cdr-raster failed to generate the outline.' }
    } finally {
        Pop-Location
    }
}

$vectorPath = Join-Path (Split-Path -Parent $Report) 'vector-output.json'
$shape = (Get-Content -LiteralPath $vectorPath -Raw | ConvertFrom-Json).shapes |
    Where-Object { $_.source_shape_path -eq $ShapePath } |
    Select-Object -First 1
if ($null -eq $shape) { throw "Missing vector output for shape $ShapePath." }

$interiorCount = @($shape.polygons | ForEach-Object { @($_.interiors).Count } |
    Measure-Object -Sum).Sum
if ($interiorCount -ne 1) {
    throw "Outline $ShapePath contains $interiorCount interior cut paths; only the generated round hole is allowed."
}

$longest = $null
foreach ($polygon in $shape.polygons) {
    $rings = [System.Collections.Generic.List[object]]::new()
    $rings.Add(@($polygon.exterior))
    foreach ($interior in $polygon.interiors) {
        $rings.Add(@($interior))
    }
    foreach ($ring in $rings) {
        for ($index = 0; $index -lt $ring.Count; $index++) {
            $next = ($index + 1) % $ring.Count
            $dx = [double]$ring[$next].x - [double]$ring[$index].x
            $dy = [double]$ring[$next].y - [double]$ring[$index].y
            $length = [Math]::Sqrt($dx * $dx + $dy * $dy)
            if ($null -eq $longest -or $length -gt $longest.LengthMm) {
                $longest = [PSCustomObject]@{
                    LengthMm = $length
                    FromX = [double]$ring[$index].x
                    FromY = [double]$ring[$index].y
                    ToX = [double]$ring[$next].x
                    ToY = [double]$ring[$next].y
                }
            }
        }

    }
}

$longest | Format-List
if ($longest.LengthMm -gt $MaximumSegmentMm) {
    throw ("Outline {0} contains a {1:N3} mm bridge; maximum allowed is {2:N3} mm." -f `
        $ShapePath, $longest.LengthMm, $MaximumSegmentMm)
}
Write-Output 'OUTLINE_TOPOLOGY=PASS'
