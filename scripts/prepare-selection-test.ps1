param(
    [string] $SourcePath = (Join-Path $PSScriptRoot '..\test.cdr'),
    [string] $WorkingCopyPath = (Join-Path $PSScriptRoot '..\target\selection-run\output.cdr')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$source = (Resolve-Path -LiteralPath $SourcePath).Path
$workingCopy = [System.IO.Path]::GetFullPath($WorkingCopyPath)
$workingDirectory = [System.IO.Path]::GetDirectoryName($workingCopy)
[System.IO.Directory]::CreateDirectory($workingDirectory) | Out-Null
Copy-Item -LiteralPath $source -Destination $workingCopy -Force

$app = New-Object -ComObject CorelDRAW.Application.22
$app.Visible = $true
$document = $app.OpenDocument($workingCopy)
$document.ClearSelection()
$page = $document.Pages.Item(1)
foreach ($shapeIndex in 8, 9, 10) {
    $shape = $page.Shapes.Item($shapeIndex)
    if ([int] $shape.Type -ne 5) {
        throw "Expected page 1 shape $shapeIndex to be a bitmap."
    }
    $shape.AddToSelection()
}

Write-Output "Opened $workingCopy with bitmap shapes 1.8, 1.9, and 1.10 selected; document not saved."
