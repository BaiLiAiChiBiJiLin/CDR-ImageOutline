param(
    [string] $SourcePath = (Join-Path $PSScriptRoot '..\target\test-run\input.cdr'),
    [string] $OutputDirectory = (Join-Path $PSScriptRoot '..\target\test-run')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$source = (Resolve-Path -LiteralPath $SourcePath).Path
$output = [System.IO.Path]::GetFullPath($OutputDirectory)
$imagesDirectory = Join-Path $output 'images'
[System.IO.Directory]::CreateDirectory($imagesDirectory) | Out-Null

$millimetersPerInch = 25.4
$interopPath = Get-ChildItem `
    'C:\Windows\Microsoft.NET\assembly\GAC_MSIL\Corel.Interop.VGCore' `
    -Recurse `
    -Filter 'Corel.Interop.VGCore.dll' |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $interopPath) {
    throw 'CorelDRAW 2020 interop assembly was not found.'
}
Add-Type -Path $interopPath

$app = New-Object -ComObject CorelDRAW.Application.22
$app.Visible = $false
$document = $null

try {
    $document = $app.OpenDocument($source)
    $items = [System.Collections.Generic.List[object]]::new()

    for ($pageIndex = 1; $pageIndex -le $document.Pages.Count; $pageIndex++) {
        $page = $document.Pages.Item($pageIndex)
        for ($shapeIndex = 1; $shapeIndex -le $page.Shapes.Count; $shapeIndex++) {
            $shape = $page.Shapes.Item($shapeIndex)
            if ([int] $shape.Type -ne 5) {
                continue
            }

            $bitmap = $shape.Bitmap
            $shapePath = "$pageIndex.$shapeIndex"
            $fileName = "bitmap-$pageIndex-$shapeIndex.png"
            $filePath = Join-Path $imagesDirectory $fileName

            $shape.CreateSelection()
            $filter = $document.ExportBitmap(
                $filePath,
                [Corel.Interop.VGCore.cdrFilter]::cdrPNG,
                [Corel.Interop.VGCore.cdrExportRange]::cdrSelection,
                [Corel.Interop.VGCore.cdrImageType]::cdrRGBColorImage,
                [int] $bitmap.SizeWidth,
                [int] $bitmap.SizeHeight,
                [int] $bitmap.ResolutionX,
                [int] $bitmap.ResolutionY,
                [Corel.Interop.VGCore.cdrAntiAliasingType]::cdrNormalAntiAliasing,
                $false,
                $true,
                $false,
                $false,
                [Corel.Interop.VGCore.cdrCompressionType]::cdrCompressionNone,
                $app.CreateStructPaletteOptions(),
                $app.CreateRect(0, 0, 0, 0)
            )
            $filter.Finish()

            $items.Add([ordered]@{
                shape_path = $shapePath
                png = "images/$fileName"
                pixel_width = [int] $bitmap.SizeWidth
                pixel_height = [int] $bitmap.SizeHeight
                dpi_x = [int] $bitmap.ResolutionX
                dpi_y = [int] $bitmap.ResolutionY
                transparent = [bool] $bitmap.Transparent
                embedded = [bool] $bitmap.Embedded
                cropped = [bool] $bitmap.Cropped
                rotation_degrees = [math]::Round([double] $shape.RotationAngle, 4)
                bbox_mm = [ordered]@{
                    left = [math]::Round([double] $shape.LeftX * $millimetersPerInch, 4)
                    bottom = [math]::Round([double] $shape.BottomY * $millimetersPerInch, 4)
                    width = [math]::Round([double] $shape.SizeWidth * $millimetersPerInch, 4)
                    height = [math]::Round([double] $shape.SizeHeight * $millimetersPerInch, 4)
                }
            })
        }
    }

    $inventory = [ordered]@{
        schema_version = 1
        source_file = [System.IO.Path]::GetFileName($source)
        source_sha256 = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
        captured_with = $app.Version
        cdr_saved = $false
        bitmap_count = $items.Count
        bitmaps = $items
    }

    $jsonPath = Join-Path $output 'inventory.json'
    $json = $inventory | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($jsonPath, "$json`r`n", [System.Text.UTF8Encoding]::new($false))
    Write-Output "Wrote $jsonPath and $($items.Count) PNG files"
} finally {
    if ($null -ne $document) {
        try { $document.Close() } catch {}
    }
    try { $app.Quit() } catch {}
}
