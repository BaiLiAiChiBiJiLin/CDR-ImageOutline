param(
    [string]$CorelInstallDir = 'D:\apps\CorelDRAW Graphics Suite 2020',
    [string]$OpenCvRoot = $env:OPENCV_DIR,
    [string]$VCRedistDir
)

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($OpenCvRoot)) {
    $OpenCvRoot = Join-Path $projectRoot 'target\opencv-4.14.0-sdk\opencv\build'
}
. (Join-Path $PSScriptRoot 'configure-opencv.ps1')
$openCvDll = Set-OpenCvEnvironment -OpenCvRoot $OpenCvRoot
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
$corelAssemblies = Join-Path $CorelInstallDir 'Programs64\Assemblies'
$interopAssembly = Join-Path $corelAssemblies 'Corel.Interop.VGCore.dll'
if (-not (Test-Path -LiteralPath $interopAssembly -PathType Leaf)) {
    throw "CorelDRAW 2020 interop assembly not found: $interopAssembly"
}

$outputRoot = Join-Path $projectRoot 'target\corel-extension'
$resolvedTarget = [IO.Path]::GetFullPath($outputRoot)
$resolvedWorkspace = [IO.Path]::GetFullPath((Join-Path $projectRoot 'target'))
if (-not $resolvedTarget.StartsWith($resolvedWorkspace, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to clean unexpected output directory: $resolvedTarget"
}
if (Test-Path -LiteralPath $resolvedTarget) {
    Remove-Item -LiteralPath $resolvedTarget -Recurse -Force
}
New-Item -ItemType Directory -Path $resolvedTarget -Force | Out-Null

Push-Location $projectRoot
try {
    & cargo build -p cdr-corel --release --features opencv
    if ($LASTEXITCODE -ne 0) { throw 'Rust release build failed.' }

    $referencePack = 'C:\Program Files (x86)\Reference Assemblies\Microsoft\Framework\.NETFramework\v4.0'
    if (Test-Path -LiteralPath $referencePack -PathType Container) {
        $msbuild = Get-Command msbuild.exe -ErrorAction SilentlyContinue
        if ($null -eq $msbuild) {
            $msbuildPath = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\MSBuild.exe'
        } else {
            $msbuildPath = $msbuild.Source
        }
        $pluginProject = Join-Path $projectRoot 'crates\cdr-plugin\CdrOutlinePlugin.csproj'
        & $msbuildPath $pluginProject /nologo /t:Rebuild /p:Configuration=Release "/p:CorelAssembliesDir=$corelAssemblies"
        if ($LASTEXITCODE -ne 0) { throw 'Corel plugin build failed.' }
        $pluginAssembly = Join-Path $projectRoot 'crates\cdr-plugin\bin\Release\CdrOutlinePlugin.dll'
    } else {
        $cscPath = 'C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
        if (-not (Test-Path -LiteralPath $cscPath -PathType Leaf)) {
            throw 'Neither a .NET Framework reference pack nor the .NET Framework compiler was found.'
        }
        $pluginAssembly = Join-Path $resolvedTarget 'CdrOutlinePlugin.dll'
        $gac = 'C:\Windows\Microsoft.NET\assembly'
        $references = @(
            (Join-Path $gac 'GAC_MSIL\System.Xaml\v4.0_4.0.0.0__b77a5c561934e089\System.Xaml.dll'),
            (Join-Path $gac 'GAC_MSIL\WindowsBase\v4.0_4.0.0.0__31bf3856ad364e35\WindowsBase.dll'),
            (Join-Path $gac 'GAC_64\PresentationCore\v4.0_4.0.0.0__31bf3856ad364e35\PresentationCore.dll'),
            (Join-Path $gac 'GAC_MSIL\PresentationFramework\v4.0_4.0.0.0__31bf3856ad364e35\PresentationFramework.dll'),
            (Join-Path $gac 'GAC_MSIL\UIAutomationTypes\v4.0_4.0.0.0__31bf3856ad364e35\UIAutomationTypes.dll'),
            $interopAssembly
        )
        $referenceArguments = $references | ForEach-Object { "/reference:$_" }
        $source = Join-Path $projectRoot 'crates\cdr-plugin\Plugin.cs'
        & $cscPath /nologo /target:library /optimize+ /platform:anycpu "/out:$pluginAssembly" $referenceArguments $source
        if ($LASTEXITCODE -ne 0) { throw 'Corel plugin fallback build failed.' }
    }
} finally {
    Pop-Location
}

$resourceSource = Join-Path $projectRoot 'crates\cdr-plugin\package\content\CdrOutlineIntl.rc'
$windowsKitsBin = 'C:\Program Files (x86)\Windows Kits\10\bin'
$resourceCompiler = Get-ChildItem -LiteralPath $windowsKitsBin -Filter rc.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\rc\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1
$visualStudioRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC'
$linker = Get-ChildItem -LiteralPath $visualStudioRoot -Filter link.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\Hostx64\\x64\\link\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1
if ($null -eq $resourceCompiler -or $null -eq $linker) {
    throw 'The Windows x64 resource compiler and linker are required to build the Corel menu resources.'
}
$resourceFile = Join-Path $resolvedTarget 'CdrOutlineIntl.res'
$resourceAssembly = Join-Path $resolvedTarget 'CdrOutlineIntl.dll'
& $resourceCompiler.FullName /nologo /c65001 "/fo$resourceFile" $resourceSource
if ($LASTEXITCODE -ne 0) { throw 'Corel menu resource compilation failed.' }
& $linker.FullName /nologo /dll /noentry /machine:x64 "/out:$resourceAssembly" $resourceFile
if ($LASTEXITCODE -ne 0) { throw 'Corel menu resource linking failed.' }

$staging = Join-Path $resolvedTarget 'unpacked'
$content = Join-Path $staging 'content'
$metaInf = Join-Path $staging 'META-INF'
New-Item -ItemType Directory -Path $content, $metaInf -Force | Out-Null

$packageSource = Join-Path $projectRoot 'crates\cdr-plugin\package'
Copy-Item -LiteralPath (Join-Path $packageSource 'content\config.xml') -Destination $content
Copy-Item -LiteralPath (Join-Path $packageSource 'content\extension.xml') -Destination $content
Copy-Item -LiteralPath (Join-Path $packageSource 'content\requirements.xml') -Destination $content
Copy-Item -LiteralPath (Join-Path $packageSource 'content\AppUI.xslt') -Destination $content
Copy-Item -LiteralPath (Join-Path $packageSource 'content\UserUI.xslt') -Destination $content
Copy-Item -LiteralPath (Join-Path $packageSource 'META-INF\container.xml') -Destination $metaInf
Copy-Item -LiteralPath (Join-Path $packageSource 'META-INF\links.xml') -Destination $metaInf
Copy-Item -LiteralPath (Join-Path $packageSource 'META-INF\metadata.xml') -Destination $metaInf

Copy-Item -LiteralPath $pluginAssembly -Destination (Join-Path $content 'CdrOutline.CorelAddon')
Copy-Item -LiteralPath $resourceAssembly -Destination $content
Copy-Item -LiteralPath (Join-Path $projectRoot 'target\release\cdr-corel.exe') -Destination $content
Copy-Item -LiteralPath $openCvDll -Destination $content
Get-ChildItem -LiteralPath $VCRedistDir -Filter '*.dll' -File | Copy-Item -Destination $content
[IO.File]::WriteAllBytes((Join-Path $content 'CorelDrw.addon'), [byte[]]::new(0))
[IO.File]::WriteAllText(
    (Join-Path $staging 'mimetype'),
    'application/x-vnd.corel.zcf.cgsaddon+zip',
    [Text.UTF8Encoding]::new($false))

$packagePath = Join-Path $resolvedTarget 'CdrOutline.CorelExtension'
Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Text;

public static class StoredZipWriter
{
    private sealed class Entry
    {
        public byte[] Name;
        public byte[] Data;
        public uint Crc;
        public uint Offset;
        public ushort Time;
        public ushort Date;
    }

    private static readonly uint[] CrcTable = CreateCrcTable();

    public static void WriteArchive(string root, string output)
    {
        var files = Directory.GetFiles(root, "*", SearchOption.AllDirectories)
            .OrderBy(path => RelativeName(root, path) == "mimetype" ? 0 : 1)
            .ThenBy(path => RelativeName(root, path), StringComparer.Ordinal)
            .ToArray();
        var entries = new List<Entry>();
        using (var stream = File.Create(output))
        using (var writer = new BinaryWriter(stream, new UTF8Encoding(false), true))
        {
            foreach (var file in files)
            {
                var name = Encoding.UTF8.GetBytes(RelativeName(root, file));
                var data = File.ReadAllBytes(file);
                if (data.LongLength > uint.MaxValue || stream.Position > uint.MaxValue)
                    throw new InvalidOperationException("ZIP64 is not supported by this package writer.");
                var stamp = File.GetLastWriteTime(file);
                var entry = new Entry {
                    Name = name,
                    Data = data,
                    Crc = ComputeCrc(data),
                    Offset = (uint)stream.Position,
                    Time = DosTime(stamp),
                    Date = DosDate(stamp)
                };
                entries.Add(entry);
                WriteLocalHeader(writer, entry);
                writer.Write(data);
            }

            var centralOffset = (uint)stream.Position;
            foreach (var entry in entries)
                WriteCentralHeader(writer, entry);
            var centralSize = (uint)stream.Position - centralOffset;
            WriteEndOfDirectory(writer, entries.Count, centralSize, centralOffset);
        }
    }

    private static string RelativeName(string root, string path)
    {
        return path.Substring(root.TrimEnd(Path.DirectorySeparatorChar).Length + 1)
            .Replace(Path.DirectorySeparatorChar, '/');
    }

    private static void WriteLocalHeader(BinaryWriter writer, Entry entry)
    {
        writer.Write(0x04034b50u);
        writer.Write((ushort)20);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write(entry.Time);
        writer.Write(entry.Date);
        writer.Write(entry.Crc);
        writer.Write((uint)entry.Data.Length);
        writer.Write((uint)entry.Data.Length);
        writer.Write((ushort)entry.Name.Length);
        writer.Write((ushort)0);
        writer.Write(entry.Name);
    }

    private static void WriteCentralHeader(BinaryWriter writer, Entry entry)
    {
        writer.Write(0x02014b50u);
        writer.Write((ushort)20);
        writer.Write((ushort)20);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write(entry.Time);
        writer.Write(entry.Date);
        writer.Write(entry.Crc);
        writer.Write((uint)entry.Data.Length);
        writer.Write((uint)entry.Data.Length);
        writer.Write((ushort)entry.Name.Length);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write(0u);
        writer.Write(entry.Offset);
        writer.Write(entry.Name);
    }

    private static void WriteEndOfDirectory(
        BinaryWriter writer,
        int entryCount,
        uint centralSize,
        uint centralOffset)
    {
        if (entryCount > ushort.MaxValue)
            throw new InvalidOperationException("Too many package entries.");
        writer.Write(0x06054b50u);
        writer.Write((ushort)0);
        writer.Write((ushort)0);
        writer.Write((ushort)entryCount);
        writer.Write((ushort)entryCount);
        writer.Write(centralSize);
        writer.Write(centralOffset);
        writer.Write((ushort)0);
    }

    private static uint ComputeCrc(byte[] data)
    {
        var crc = 0xffffffffu;
        foreach (var value in data)
            crc = CrcTable[(crc ^ value) & 0xff] ^ (crc >> 8);
        return crc ^ 0xffffffffu;
    }

    private static uint[] CreateCrcTable()
    {
        var table = new uint[256];
        for (uint index = 0; index < table.Length; index++)
        {
            var value = index;
            for (var bit = 0; bit < 8; bit++)
                value = (value & 1) != 0 ? 0xedb88320u ^ (value >> 1) : value >> 1;
            table[index] = value;
        }
        return table;
    }

    private static ushort DosTime(DateTime value)
    {
        return (ushort)((value.Hour << 11) | (value.Minute << 5) | (value.Second / 2));
    }

    private static ushort DosDate(DateTime value)
    {
        var year = Math.Max(1980, value.Year) - 1980;
        return (ushort)((year << 9) | (value.Month << 5) | value.Day);
    }
}
'@
[StoredZipWriter]::WriteArchive($staging, $packagePath)

$hash = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash
Write-Output "Corel extension: $packagePath"
Write-Output "SHA-256: $hash"
