param(
    [string] $SourcePath = (Join-Path $PSScriptRoot '..\Backup_of_naomi hannah.cdr'),
    [string] $OutputPath = (Join-Path $PSScriptRoot '..\fixtures\sample-002\expected-holes.json')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$millimetersPerInch = 25.4

function Convert-ToMillimeters([double] $value) {
    return [math]::Round($value * $millimetersPerInch, 4)
}

function Convert-ToSquareMillimeters([double] $value) {
    return [math]::Round($value * $millimetersPerInch * $millimetersPerInch, 6)
}

function Get-Point([double] $x, [double] $y) {
    return [ordered]@{
        x = Convert-ToMillimeters $x
        y = Convert-ToMillimeters $y
    }
}

function Get-BoundingBox($shape) {
    return [ordered]@{
        left = Convert-ToMillimeters $shape.LeftX
        bottom = Convert-ToMillimeters $shape.BottomY
        width = Convert-ToMillimeters $shape.SizeWidth
        height = Convert-ToMillimeters $shape.SizeHeight
    }
}

function Get-Center($shape) {
    return Get-Point `
        ($shape.LeftX + ($shape.SizeWidth / 2.0)) `
        ($shape.BottomY + ($shape.SizeHeight / 2.0))
}

function Get-SubPaths($shape) {
    $subPaths = [System.Collections.Generic.List[object]]::new()
    foreach ($subPath in $shape.Curve.SubPaths) {
        $segments = [System.Collections.Generic.List[object]]::new()
        foreach ($segment in $subPath.Segments) {
            $entry = [ordered]@{
                type = [int] $segment.Type
                start = Get-Point $segment.StartNode.PositionX $segment.StartNode.PositionY
            }
            if ([int] $segment.Type -eq 1) {
                $entry.control_start = Get-Point $segment.StartingControlPointX $segment.StartingControlPointY
                $entry.control_end = Get-Point $segment.EndingControlPointX $segment.EndingControlPointY
            }
            $entry.end = Get-Point $segment.EndNode.PositionX $segment.EndNode.PositionY
            $segments.Add($entry)
        }

        $subPaths.Add([ordered]@{
            closed = [bool] $subPath.Closed
            clockwise = [bool] $subPath.IsClockwise
            area_mm2 = Convert-ToSquareMillimeters $subPath.Area
            segments = $segments
        })
    }
    return $subPaths
}

function Get-OutlineHex($shape) {
    if ([int] $shape.Outline.Type -eq 0) {
        return $null
    }
    $hex = [string] $shape.Outline.Color.HexValue
    if ($hex.StartsWith('#')) {
        return $hex
    }
    return "#$hex"
}

function Test-SameBounds($left, $right) {
    $epsilon = 0.00001
    return [math]::Abs($left.LeftX - $right.LeftX) -le $epsilon -and
        [math]::Abs($left.BottomY - $right.BottomY) -le $epsilon -and
        [math]::Abs($left.SizeWidth - $right.SizeWidth) -le $epsilon -and
        [math]::Abs($left.SizeHeight - $right.SizeHeight) -le $epsilon
}

$resolvedSource = (Resolve-Path -LiteralPath $SourcePath).Path
$resolvedOutput = [System.IO.Path]::GetFullPath($OutputPath)
$sourceHash = (Get-FileHash -LiteralPath $resolvedSource -Algorithm SHA256).Hash

$app = New-Object -ComObject CorelDRAW.Application.22
$app.Visible = $false
$document = $null

try {
    $document = $app.OpenDocument($resolvedSource)
    $sourcePage = $document.Pages.Item(6)
    $outputPage = $document.Pages.Item(7)

    $sourceShapes = @(
        for ($index = 1; $index -le $sourcePage.Shapes.Count; $index++) {
            $shape = $sourcePage.Shapes.Item($index)
            if ([int] $shape.Type -eq 3 -and $shape.Name -eq 'h_xbxb') {
                [pscustomobject]@{ Path = "6.$index"; Shape = $shape }
            }
        }
    )
    $outputShapes = @(
        for ($index = 1; $index -le $outputPage.Shapes.Count; $index++) {
            $shape = $outputPage.Shapes.Item($index)
            if ([int] $shape.Type -eq 3 -and $shape.Name -eq 'h_xbxb') {
                [pscustomobject]@{ Path = "7.$index"; Shape = $shape }
            }
        }
    )
    $holeShapes = @(
        for ($index = 1; $index -le $outputPage.Shapes.Count; $index++) {
            $shape = $outputPage.Shapes.Item($index)
            if ([int] $shape.Type -eq 3 -and $shape.Name -eq '' -and
                [math]::Abs((Convert-ToMillimeters $shape.SizeWidth) - 3.0) -le 0.001 -and
                [math]::Abs((Convert-ToMillimeters $shape.SizeHeight) - 3.0) -le 0.001) {
                [pscustomobject]@{ Path = "7.$index"; Shape = $shape }
            }
        }
    )

    if ($sourceShapes.Count -ne 32 -or $outputShapes.Count -ne 32 -or $holeShapes.Count -ne 32) {
        throw "Expected 32 source paths, 32 output paths, and 32 holes; found $($sourceShapes.Count), $($outputShapes.Count), and $($holeShapes.Count)."
    }

    $cases = [System.Collections.Generic.List[object]]::new()
    for ($index = 0; $index -lt $outputShapes.Count; $index++) {
        $output = $outputShapes[$index]
        $hole = $holeShapes[$index]
        $matchingSources = @($sourceShapes | Where-Object { Test-SameBounds $_.Shape $output.Shape })
        if ($matchingSources.Count -ne 1) {
            throw "Expected one source match for $($output.Path); found $($matchingSources.Count)."
        }

        $holeCenterX = $hole.Shape.LeftX + ($hole.Shape.SizeWidth / 2.0)
        $holeCenterY = $hole.Shape.BottomY + ($hole.Shape.SizeHeight / 2.0)
        $inside = $false
        foreach ($subPath in $output.Shape.Curve.SubPaths) {
            if ($subPath.IsPointInside($holeCenterX, $holeCenterY)) {
                $inside = $true
                break
            }
        }
        if (-not $inside) {
            throw "Hole $($hole.Path) is not inside output $($output.Path)."
        }

        $source = $matchingSources[0]
        $cases.Add([ordered]@{
            id = "hole-$($output.Path.Replace('.', '-'))"
            source = [ordered]@{
                shape_path = $source.Path
                name = $source.Shape.Name
                bbox_mm = Get-BoundingBox $source.Shape
            }
            expected = [ordered]@{
                outline = [ordered]@{
                    shape_path = $output.Path
                    name = $output.Shape.Name
                    bbox_mm = Get-BoundingBox $output.Shape
                    outline_width_mm = Convert-ToMillimeters $output.Shape.Outline.Width
                    outline_hex = Get-OutlineHex $output.Shape
                    fill_type = [int] $output.Shape.Fill.Type
                    subpaths = @(Get-SubPaths $output.Shape)
                }
                hole = [ordered]@{
                    shape_path = $hole.Path
                    center_mm = Get-Center $hole.Shape
                    diameter_mm = Convert-ToMillimeters (($hole.Shape.SizeWidth + $hole.Shape.SizeHeight) / 2.0)
                    bbox_mm = Get-BoundingBox $hole.Shape
                    outline_width_mm = Convert-ToMillimeters $hole.Shape.Outline.Width
                    outline_hex = Get-OutlineHex $hole.Shape
                    fill_type = [int] $hole.Shape.Fill.Type
                    matched_by = 'center_inside_outline'
                    subpaths = @(Get-SubPaths $hole.Shape)
                }
            }
        })
    }

    $fixture = [ordered]@{
        schema_version = 1
        source_file = [System.IO.Path]::GetFileName($resolvedSource)
        source_sha256 = $sourceHash
        source_pages = [ordered]@{ inputs = 6; expected_outputs = 7 }
        captured_with = $app.Version
        source_document_unit = 'inch'
        fixture_unit = 'millimeter'
        configured_hole_diameter_mm = 3.0
        configured_edge_clearance_mm = 2.0
        parameter_status = 'user_confirmed'
        cases = $cases
    }

    $outputDirectory = Split-Path -Parent $resolvedOutput
    [System.IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
    $json = $fixture | ConvertTo-Json -Depth 100
    [System.IO.File]::WriteAllText($resolvedOutput, "$json`n", [System.Text.UTF8Encoding]::new($false))
} finally {
    if ($null -ne $document) {
        try { $document.Close() } catch {}
    }
    try { $app.Quit() } catch {}
}

Write-Output "Wrote $resolvedOutput"
