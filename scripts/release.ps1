param(
    [ValidateSet('patch', 'minor', 'major')]
    [string]$Bump = 'patch',
    [string]$Remote = 'origin',
    [string]$Branch,
    [string]$CommitMessage,
    [string]$CorelInstallDir = 'D:\apps\CorelDRAW Graphics Suite 2020',
    [string]$OpenCvRoot = $env:OPENCV_DIR,
    [string]$VCRedistDir,
    [int]$Jobs = 1,
    [switch]$Push,
    [switch]$PublishRelease,
    [switch]$CreateIssue,
    [switch]$NoBuild,
    [switch]$WhatIf
)

<#
.SYNOPSIS
    自动递增版本、构建 Windows 便携包，并可提交/发布 GitHub Release。

.EXAMPLE
    .\scripts\release.ps1 -Push

.EXAMPLE
    .\scripts\release.ps1 -Push -PublishRelease

.EXAMPLE
    .\scripts\release.ps1 -Push -PublishRelease -CreateIssue

.EXAMPLE
    .\scripts\release.ps1 -WhatIf

说明：
  - 默认递增补丁版本：0.1.25 -> 0.1.26。
  - 默认会构建本地便携包并提交本地 Git commit；只有传入 -Push 才会推送到 GitHub。
  - -PublishRelease 需要已安装并登录 GitHub CLI（gh），会上传 ZIP 和 SHA256SUMS.txt。
  - -CreateIssue 会在 Release 创建后新建 Issue；它使用本次 Git commit 信息作为版本说明。
  - 新版本发布不会覆盖已存在的产物；工作区干净时允许覆盖当前版本的 target/portable 生成物以便重建。
  - 便携目录、ZIP 和 SHA256SUMS.txt 中的 ZIP 文件名统一使用 CDR-ImageOutline-<version>。
#>

$ErrorActionPreference = 'Stop'

function Write-Utf8NoBom {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Content
    )
    [IO.File]::WriteAllText($Path, $Content, [Text.UTF8Encoding]::new($false))
}

function Replace-ExactlyOnce {
    param(
        [Parameter(Mandatory = $true)][string]$Text,
        [Parameter(Mandatory = $true)][string]$Pattern,
        [Parameter(Mandatory = $true)][string]$Replacement,
        [Parameter(Mandatory = $true)][string]$Description
    )
    $regex = [regex]::new($Pattern)
    $count = $regex.Matches($Text).Count
    if ($count -ne 1) {
        throw "无法安全更新 $Description：预期匹配 1 次，实际匹配 $count 次。"
    }
    return $regex.Replace($Text, $Replacement, 1)
}

function Invoke-Git {
    param([Parameter(Mandatory = $true)][string[]]$Arguments)
    & git @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments -join ' ') 执行失败，退出码 $LASTEXITCODE。"
    }
}

$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$desktopManifestPath = Join-Path $projectRoot 'crates\cdr-desktop\Cargo.toml'
$cargoLockPath = Join-Path $projectRoot 'Cargo.lock'
$extensionManifestPath = Join-Path $projectRoot 'crates\cdr-plugin\package\content\extension.xml'
$metadataPath = Join-Path $projectRoot 'crates\cdr-plugin\package\META-INF\metadata.xml'

foreach ($required in @($desktopManifestPath, $cargoLockPath, $extensionManifestPath, $metadataPath)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "缺少版本文件：$required"
    }
}

Push-Location $projectRoot
try {
    $insideWorkTree = (& git rev-parse --is-inside-work-tree 2>$null).Trim()
    if ($insideWorkTree -ne 'true') {
        throw '当前目录不是 Git 工作区。'
    }

    if ([string]::IsNullOrWhiteSpace($Branch)) {
        $Branch = (& git branch --show-current).Trim()
    }
    if ([string]::IsNullOrWhiteSpace($Branch)) {
        throw '无法确定当前 Git 分支；请使用 -Branch 指定。'
    }

    $hasPendingChanges = @(& git status --porcelain).Count -gt 0

    $desktopManifest = [IO.File]::ReadAllText($desktopManifestPath)
    $versionMatch = [regex]::Match($desktopManifest, '(?m)^version\s*=\s*"([0-9]+)\.([0-9]+)\.([0-9]+)"\s*$')
    if (-not $versionMatch.Success) {
        throw '无法从 cdr-desktop/Cargo.toml 读取三段式版本号。'
    }

    $major = [int]$versionMatch.Groups[1].Value
    $minor = [int]$versionMatch.Groups[2].Value
    $patch = [int]$versionMatch.Groups[3].Value
    switch ($Bump) {
        'major' { $major++; $minor = 0; $patch = 0 }
        'minor' { $minor++; $patch = 0 }
        'patch' { $patch++ }
    }
    $currentVersion = '{0}.{1}.{2}' -f $versionMatch.Groups[1].Value, $versionMatch.Groups[2].Value, $versionMatch.Groups[3].Value
    if ($hasPendingChanges) {
        $newVersion = '{0}.{1}.{2}' -f $major, $minor, $patch
    } else {
        $newVersion = $currentVersion
        Write-Output "Git 工作区没有可提交的修改，将保持版本 $currentVersion 并继续构建。"
    }
    if ([string]::IsNullOrWhiteSpace($CommitMessage)) {
        $CommitMessage = "release: v$newVersion"
    }

    $lockText = [IO.File]::ReadAllText($cargoLockPath)
    # Cargo.lock 的版本行后面通常紧接 dependencies；使用更严格的 package 片段避免误改依赖版本。
    $lockPackagePattern = '(?s)(\[\[package\]\]\s*\r?\nname\s*=\s*"cdr-desktop"\s*\r?\nversion\s*=\s*")[^"]+(")'
    $lockMatch = [regex]::Match($lockText, $lockPackagePattern)
    if (-not $lockMatch.Success) {
        throw '无法在 Cargo.lock 中找到 cdr-desktop 包版本。'
    }
    if ($lockMatch.Groups[0].Value -notmatch [regex]::Escape($currentVersion)) {
        throw "Cargo.lock 中 cdr-desktop 版本与 Cargo.toml 不一致，当前 Cargo.toml 为 $currentVersion。"
    }

    $extensionText = [IO.File]::ReadAllText($extensionManifestPath)
    $extensionVersionMatch = [regex]::Match($extensionText, 'extension_version="([^"]+)"')
    if (-not $extensionVersionMatch.Success -or $extensionVersionMatch.Groups[1].Value -ne $currentVersion) {
        throw "extension.xml 版本与桌面程序版本不一致，预期为 $currentVersion。"
    }
    $metadataText = [IO.File]::ReadAllText($metadataPath)
    $metadataVersionMatch = [regex]::Match($metadataText, '<crl:ContentVersion>([^<]+)</crl:ContentVersion>')
    if (-not $metadataVersionMatch.Success -or $metadataVersionMatch.Groups[1].Value -ne $currentVersion) {
        throw "metadata.xml 版本与桌面程序版本不一致，预期为 $currentVersion。"
    }

    Write-Output "版本：$currentVersion -> $newVersion"
    Write-Output "分支：$Branch；构建并行度：$Jobs"
    if ($Push) { Write-Output "推送：$Remote/$Branch" } else { Write-Output '推送：否（使用 -Push 才会推送）' }
    if ($PublishRelease) { Write-Output 'GitHub Release：是' }

    if (($PublishRelease -or $CreateIssue) -and -not $WhatIf) {
        if (-not $Push) {
            throw '-PublishRelease 或 -CreateIssue 需要同时指定 -Push。'
        }
        if ($CreateIssue -and -not $PublishRelease) {
            throw '-CreateIssue 需要同时指定 -PublishRelease。'
        }
        if ($null -eq (Get-Command gh -ErrorAction SilentlyContinue)) {
            throw '未找到 GitHub CLI（gh）；请安装并执行 gh auth login 后重试。'
        }
    }

    if ($WhatIf) {
        if (-not $hasPendingChanges) {
            Write-Output 'WhatIf：工作区干净，不递增版本，也不构建。'
            return
        }
        Write-Output 'WhatIf：不会修改版本、构建、提交或推送。'
        return
    }

    if (($PublishRelease -or $CreateIssue) -and -not $hasPendingChanges) {
        throw '当前版本没有新的 Git 修改，不能创建 GitHub Release 或 New Issue；如需发布新版本，请先提交代码或文档修改。'
    }

    if ($hasPendingChanges) {
        $newDesktopManifest = Replace-ExactlyOnce `
        -Text $desktopManifest `
        -Pattern '(?m)^(version\s*=\s*")[0-9]+\.[0-9]+\.[0-9]+("\s*)$' `
        -Replacement ('${1}' + $newVersion + '${2}') `
        -Description 'crates/cdr-desktop/Cargo.toml 版本'
        $newLockText = Replace-ExactlyOnce `
        -Text $lockText `
        -Pattern $lockPackagePattern `
        -Replacement ('${1}' + $newVersion + '${2}') `
        -Description 'Cargo.lock cdr-desktop 版本'
        $newExtensionText = Replace-ExactlyOnce `
        -Text $extensionText `
        -Pattern '(extension_version=")[^"]+(")' `
        -Replacement ('${1}' + $newVersion + '${2}') `
        -Description 'extension.xml 版本'
        $newMetadataText = Replace-ExactlyOnce `
        -Text $metadataText `
        -Pattern '(<crl:ContentVersion>)[^<]+(</crl:ContentVersion>)' `
        -Replacement ('${1}' + $newVersion + '${2}') `
        -Description 'metadata.xml 版本'

        Write-Utf8NoBom $desktopManifestPath $newDesktopManifest
        Write-Utf8NoBom $cargoLockPath $newLockText
        Write-Utf8NoBom $extensionManifestPath $newExtensionText
        Write-Utf8NoBom $metadataPath $newMetadataText
    }

    if (-not $NoBuild) {
        $env:CARGO_BUILD_JOBS = [Math]::Max(1, $Jobs).ToString()
        $env:CARGO_INCREMENTAL = '0'
        $buildScript = Join-Path $PSScriptRoot 'build-portable.ps1'
        $buildParameters = @{}
        if (-not [string]::IsNullOrWhiteSpace($CorelInstallDir)) { $buildParameters.CorelInstallDir = $CorelInstallDir }
        if (-not [string]::IsNullOrWhiteSpace($OpenCvRoot)) { $buildParameters.OpenCvRoot = $OpenCvRoot }
        if (-not [string]::IsNullOrWhiteSpace($VCRedistDir)) { $buildParameters.VCRedistDir = $VCRedistDir }
        if (-not $hasPendingChanges) { $buildParameters.Force = $true }
        & $buildScript @buildParameters
        if ($LASTEXITCODE -ne 0) { throw '便携包构建失败；版本文件已更新但尚未提交。' }
    } else {
        Write-Output '已跳过构建（-NoBuild）。'
    }

    $portableRoot = Join-Path $projectRoot 'target\portable'
    $distribution = Join-Path $portableRoot "CDR-ImageOutline-$newVersion"
    $archive = Join-Path $portableRoot "CDR-ImageOutline-$newVersion-Windows-x64.zip"
    $checksums = Join-Path $portableRoot 'SHA256SUMS.txt'
    if (-not $NoBuild) {
        foreach ($artifact in @($distribution, $archive, $checksums)) {
            if (-not (Test-Path -LiteralPath $artifact -PathType Leaf -ErrorAction SilentlyContinue) -and
                -not (Test-Path -LiteralPath $artifact -PathType Container -ErrorAction SilentlyContinue)) {
                throw "构建完成但缺少产物：$artifact"
            }
        }
    }

    if ($hasPendingChanges) {
        Invoke-Git @('add', '-A')
        $staged = (& git diff --cached --name-only).Trim()
        if ([string]::IsNullOrWhiteSpace($staged)) {
            throw '没有可提交的变更。'
        }
        Invoke-Git @('commit', '-m', $CommitMessage)

        if ($Push) {
            Invoke-Git @('push', $Remote, $Branch)
        }
    } else {
        Write-Output '当前版本构建完成；没有 Git 修改，不执行提交、推送或 Release。'
    }

    $releaseUrl = $null
    if ($PublishRelease) {
        if (-not (Test-Path -LiteralPath $archive -PathType Leaf) -or
            -not (Test-Path -LiteralPath $checksums -PathType Leaf)) {
            throw '发布 Release 需要先成功生成 ZIP 和 SHA256SUMS.txt。'
        }
        $gh = Get-Command gh -ErrorAction Stop
        $repository = (& git config --get "remote.$Remote.url").Trim()
        if ([string]::IsNullOrWhiteSpace($repository)) {
            throw "无法读取 remote.$Remote 的 GitHub 地址。"
        }
        & $gh.Source release create "v$newVersion" $archive $checksums `
            --repo $repository `
            --title "CDR 巡边工具 v$newVersion" `
            --notes $CommitMessage
        if ($LASTEXITCODE -ne 0) {
            throw 'GitHub Release 创建失败；代码提交已完成，可稍后手动重试发布。'
        }
        $releaseUrl = (& $gh.Source release view "v$newVersion" --repo $repository --json url --jq '.url').Trim()
        if ([string]::IsNullOrWhiteSpace($releaseUrl)) {
            throw 'Release 已创建，但无法读取 Release 链接。'
        }
        Write-Output "GitHub Release：$releaseUrl"
    }

    if ($CreateIssue) {
        if ([string]::IsNullOrWhiteSpace($releaseUrl)) {
            throw '创建 New Issue 前没有可用的 Release 链接。'
        }
        $commitSha = (& git rev-parse --short HEAD).Trim()
        $issueTitle = "CDR 巡边工具 v$newVersion 发布"
        $issueBody = @"
## 版本说明

$CommitMessage

## 发布信息

- 版本：v$newVersion
- Git 提交：$commitSha
- Release：[$releaseUrl]($releaseUrl)
- SHA-256：请在 Release 中下载 SHA256SUMS.txt 校验 ZIP。

## 下载

请从 [GitHub Release]($releaseUrl) 下载 Windows x64 便携包。
"@
        $issueUrl = (& $gh.Source issue create --repo $repository --title $issueTitle --body $issueBody).Trim()
        if ($LASTEXITCODE -ne 0) {
            throw 'Release 已创建，但 New Issue 创建失败；可稍后手动创建 Issue。'
        }
        Write-Output "GitHub Issue：$issueUrl"
    }

    Write-Output "完成：v$newVersion"
    if (-not $NoBuild) {
        Write-Output "便携包：$archive"
        Write-Output "校验文件：$checksums"
    }
    if (-not $Push) {
        Write-Output '提示：尚未推送到 GitHub；确认无误后执行 git push，或下次使用 -Push。'
    }
} finally {
    Pop-Location
}
