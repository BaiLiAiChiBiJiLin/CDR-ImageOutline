function Set-OpenCvEnvironment {
    param(
        [Parameter(Mandatory = $true)]
        [string]$OpenCvRoot
    )

    if (-not (Test-Path -LiteralPath $OpenCvRoot -PathType Container)) {
        throw "OpenCV 4.14 SDK directory not found: $OpenCvRoot"
    }

    $worldDll = Get-ChildItem -LiteralPath $OpenCvRoot -Filter 'opencv_world4140.dll' -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $worldLib = Get-ChildItem -LiteralPath $OpenCvRoot -Filter 'opencv_world4140.lib' -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $includePath = Join-Path $OpenCvRoot 'include'
    if ($null -eq $worldDll -or $null -eq $worldLib -or -not (Test-Path -LiteralPath $includePath -PathType Container)) {
        throw 'Expected OpenCV 4.14 files were not found (include/, opencv_world4140.lib, opencv_world4140.dll).'
    }

    $env:OPENCV_DIR = [IO.Path]::GetFullPath($OpenCvRoot)
    $env:OPENCV_INCLUDE_PATHS = [IO.Path]::GetFullPath($includePath)
    $env:OPENCV_LINK_PATHS = $worldLib.Directory.FullName
    $env:OPENCV_LINK_LIBS = 'opencv_world4140'
    $env:PATH = $worldDll.Directory.FullName + ';' + $env:PATH

    $clangCandidates = @(
        $env:LIBCLANG_PATH,
        'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\Llvm\x64\bin',
        'C:\Program Files\LLVM\bin'
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    $clangBin = $clangCandidates |
        Where-Object { Test-Path -LiteralPath (Join-Path $_ 'libclang.dll') } |
        Select-Object -First 1
    if ($null -eq $clangBin) {
        throw 'LLVM/Clang with libclang.dll is required to generate Rust OpenCV bindings. Add the Visual Studio LLVM/Clang component.'
    }
    $clangBin = [IO.Path]::GetFullPath($clangBin)
    $clangExe = Join-Path $clangBin 'clang.exe'
    if (-not (Test-Path -LiteralPath $clangExe -PathType Leaf)) {
        throw "clang.exe not found beside libclang.dll: $clangBin"
    }
    $env:LIBCLANG_PATH = $clangBin
    $env:CLANG_PATH = $clangExe
    $env:PATH = $clangBin + ';' + $env:PATH
    return $worldDll.FullName
}
