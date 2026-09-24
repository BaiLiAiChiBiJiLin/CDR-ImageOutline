param(
    [string]$CorelInstallDir = 'D:\apps\CorelDRAW Graphics Suite 2020',
    [string]$OpenCvRoot = $env:OPENCV_DIR,
    [string]$VCRedistDir
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$targetRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'target'))
if ([string]::IsNullOrWhiteSpace($OpenCvRoot)) {
    $OpenCvRoot = Join-Path $targetRoot 'opencv-4.14.0-sdk\opencv\build'
}
if ([string]::IsNullOrWhiteSpace($VCRedistDir)) {
    $redistRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Redist\MSVC'
    $VCRedistDir = Get-ChildItem -LiteralPath $redistRoot -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName 'x64\Microsoft.VC143.CRT' } |
        Where-Object { Test-Path -LiteralPath $_ -PathType Container } |
        Select-Object -First 1
}
if ([string]::IsNullOrWhiteSpace($VCRedistDir) -or -not (Test-Path -LiteralPath $VCRedistDir -PathType Container)) {
    throw 'The x64 Microsoft VC143 app-local runtime DLLs were not found.'
}

& (Join-Path $PSScriptRoot 'build-corel-extension.ps1') `
    -CorelInstallDir $CorelInstallDir `
    -OpenCvRoot $OpenCvRoot `
    -VCRedistDir $VCRedistDir
if ($LASTEXITCODE -ne 0) { throw 'Corel extension build failed.' }

& (Join-Path $PSScriptRoot 'build-desktop.ps1')
if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed.' }

$portableRoot = Join-Path $targetRoot 'portable'
$desktopManifest = Get-Content -LiteralPath (Join-Path $projectRoot 'crates\cdr-desktop\Cargo.toml') -Raw
$versionMatch = [regex]::Match($desktopManifest, '(?m)^version\s*=\s*"([^\"]+)"')
if (-not $versionMatch.Success) { throw 'Could not determine the desktop release version.' }
$releaseTag = 'CDR巡边工具-{0}' -f $versionMatch.Groups[1].Value
$distribution = Join-Path $portableRoot $releaseTag
$archive = Join-Path $portableRoot ($releaseTag + '-Windows-x64.zip')
foreach ($candidate in @($distribution, $archive)) {
    $resolved = [IO.Path]::GetFullPath($candidate)
    if (-not $resolved.StartsWith($targetRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to replace a path outside target/: $resolved"
    }
    if (Test-Path -LiteralPath $resolved) {
        throw "Refusing to overwrite an existing portable release: $resolved"
    }
}
New-Item -ItemType Directory -Path $distribution -Force | Out-Null

$releaseDir = Join-Path $targetRoot 'release'
$extensionPackage = Join-Path $targetRoot 'corel-extension\CdrOutline.CorelExtension'
$openCvDll = Get-ChildItem -LiteralPath $OpenCvRoot -Filter 'opencv_world4140.dll' -Recurse -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -eq $openCvDll) { throw "OpenCV runtime DLL not found below $OpenCvRoot" }
foreach ($source in @(
    (Join-Path $releaseDir 'cdr-desktop.exe'),
    (Join-Path $releaseDir 'cdr-corel.exe'),
    $extensionPackage
)) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Required release file missing: $source" }
}

Copy-Item -LiteralPath (Join-Path $releaseDir 'cdr-desktop.exe') -Destination $distribution
Copy-Item -LiteralPath (Join-Path $releaseDir 'cdr-corel.exe') -Destination $distribution
Copy-Item -LiteralPath $openCvDll.FullName -Destination $distribution
Get-ChildItem -LiteralPath $VCRedistDir -Filter '*.dll' -File | Copy-Item -Destination $distribution
Copy-Item -LiteralPath $extensionPackage -Destination $distribution

$readme = @'
CDR 巡边工具 · Windows x64 便携版

使用方法
1. 将本文件夹完整解压到本地磁盘，不要只复制其中的 EXE。
2. 运行 cdr-desktop.exe。处理器、OpenCV、VC 运行库和 Corel 插件包均已随包提供。
3. 首次使用 Corel 插件时，在窗口中选择 CorelDRAW 2020 安装目录并点击“安装 / 更新 Corel 插件”。
4. 安装后重启 CorelDRAW；在 CorelDRAW 中选择对象，再回到桌面窗口使用相应功能。
5. “透明 SVG”区域提供“导出选中为 SVG”和“导出页面为 SVG”；默认写入桌面，同名 SVG 会替换。勾选 PrintFlow 选项后，导出完成会提交到本机 PrintFlow API。

运行要求
- 64 位 Windows 10 或更新版本。
- CorelDRAW Graphics Suite 2020（插件及处理当前选择功能需要）。
- 不需要在目标电脑安装 Rust、Visual Studio、OpenCV SDK 或单独安装 VC 运行库。

说明
- OpenCV 与 VC 运行时采用应用本地部署，文件都放在本目录；Windows 系统 UCRT 仍由 Windows 自身提供。
- 安装/更新会把旧插件移动到 CorelDRAW\Extensions\_CdrOutlineBackups 以便恢复；卸载也只移动到备份目录。
- 如果 CorelDRAW 安装在受保护目录（例如 Program Files）而写入被拒绝，请关闭后以管理员身份重新运行桌面程序。
- 工具不会自动关闭 CorelDRAW，也不会自动保存或覆盖 CDR 文档。
- PrintFlow 未启动时，SVG 仍会正常导出，只显示本地 API 不可用的提示。
'@
[IO.File]::WriteAllText((Join-Path $distribution '使用说明.txt'), $readme, [Text.UTF8Encoding]::new($false))

Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory(
    $distribution,
    $archive,
    [IO.Compression.CompressionLevel]::Optimal,
    $false)

$checksum = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(
    (Join-Path $portableRoot 'SHA256SUMS.txt'),
    "$checksum  $([IO.Path]::GetFileName($archive))`n",
    [Text.UTF8Encoding]::new($false))

Write-Output "Portable folder: $distribution"
Write-Output "Portable ZIP: $archive"
Write-Output "SHA-256: $checksum"
Write-Output "Plugin SHA-256: $((Get-FileHash -LiteralPath (Join-Path $distribution 'CdrOutline.CorelExtension') -Algorithm SHA256).Hash)"
