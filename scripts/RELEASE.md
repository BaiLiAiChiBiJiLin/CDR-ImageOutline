# 一键发布与打包

`release.ps1` 用于创建 Windows 新版本：递增桌面程序和 Corel 插件版本，构建便携包，生成 SHA-256 校验文件，并按需提交到 Git 和发布到 GitHub Release。

脚本默认只创建本地 Git commit，不会自动推送。确认产物和提交内容无误后，再使用 `-Push` 推送到 GitHub。

脚本只在工作区存在未提交修改时递增版本。工作区干净时，脚本保持当前版本并继续构建；它不提交、不推送，也不创建新的 GitHub Release。

## 快速开始

在项目根目录打开 PowerShell：

```powershell
.\scripts\release.ps1 -Push
```

脚本默认递增补丁版本，例如：

```text
0.1.25 -> 0.1.26
```

产物位于：

```text
target\portable\CDR-ImageOutline-0.1.26\
target\portable\CDR-ImageOutline-0.1.26-Windows-x64.zip
target\portable\SHA256SUMS.txt
```

便携目录、ZIP 和 `SHA256SUMS.txt` 中记录的 ZIP 文件名统一使用 `CDR-ImageOutline-<版本号>` 前缀。校验文件中的 ZIP 名称必须与 Release 上传的资产名称完全一致。

如果需要同时创建 GitHub Release 并上传 ZIP 和校验文件：

```powershell
gh auth login
.\scripts\release.ps1 -Push -PublishRelease
```

如果还要自动创建 New Issue，并把本次 Git commit 信息作为版本说明：

```powershell
gh auth login
.\scripts\release.ps1 -Push -PublishRelease -CreateIssue
```

脚本会在 Issue 中写入版本号、短提交哈希、Release 链接和校验说明。

自动更新依赖 GitHub Release 中同时存在以下资产：

- `CDR-ImageOutline-0.1.26-Windows-x64.zip`
- `SHA256SUMS.txt`

只推送 Git commit 不会产生可供自动更新下载的安装包。

## 执行前准备

需要以下环境：

- Windows PowerShell 5.1 或 PowerShell 7。
- Git，并且当前目录是项目 Git 工作区。
- Rust/Cargo 工具链。
- CorelDRAW 2020 及其 interop 程序集。
- OpenCV SDK 和 x64 VC 运行库。

默认路径如下：

| 依赖 | 默认路径 |
| --- | --- |
| CorelDRAW | `D:\apps\CorelDRAW Graphics Suite 2020` |
| OpenCV SDK | `target\opencv-4.14.0-sdk\opencv\build`，也可以使用 `OPENCV_DIR` |
| VC 运行库 | Visual Studio 2022 Build Tools 的 x64 VC143 目录 |

依赖安装在其他位置时：

```powershell
.\scripts\release.ps1 `
  -Push `
  -CorelInstallDir 'D:\apps\CorelDRAW Graphics Suite 2020' `
  -OpenCvRoot 'D:\sdk\opencv\build' `
  -VCRedistDir 'C:\runtime\Microsoft.VC143.CRT'
```

脚本会执行 `git add -A`，提交当前工作区中所有未忽略的修改。正式发布前请检查：

```powershell
git status --short
git branch --show-current
git remote -v
```

## 参数

| 参数 | 默认值 | 作用 |
| --- | --- | --- |
| `-Bump patch\|minor\|major` | `patch` | 选择补丁、次版本或主版本递增。 |
| `-Push` | 不启用 | 将提交推送到远程 Git 仓库。 |
| `-PublishRelease` | 不启用 | 使用 GitHub CLI 创建 Release 并上传 ZIP、校验文件；必须同时使用 `-Push`。 |
| `-CreateIssue` | 不启用 | 在 Release 创建后新建 Issue；使用本次 Git commit 信息作为版本说明，必须同时使用 `-Push -PublishRelease`。 |
| `-WhatIf` | 不启用 | 只显示版本和分支，不修改文件、不构建、不提交。 |
| `-NoBuild` | 不启用 | 只更新版本并提交，不构建便携包。仅适合调试。 |
| `-Remote` | `origin` | Git 远程名称。 |
| `-Branch` | 当前分支 | 要推送的分支。 |
| `-CommitMessage` | `release: v<新版本>` | Git commit 消息。 |
| `-Jobs` | `1` | Cargo 构建并行任务数。内存较小时保持为 `1`。 |
| `-CorelInstallDir` | CorelDRAW 默认路径 | CorelDRAW 安装目录。 |
| `-OpenCvRoot` | `OPENCV_DIR` 或项目内 SDK | OpenCV SDK 的 `build` 目录。 |
| `-VCRedistDir` | 自动查找 | x64 VC143 运行库目录。 |

常用命令：

```powershell
# 查看版本变化，不做任何修改
.\scripts\release.ps1 -WhatIf

# 发布次版本
.\scripts\release.ps1 -Bump minor -Push

# 发布主版本
.\scripts\release.ps1 -Bump major -Push

# 自定义提交消息
.\scripts\release.ps1 -Push -CommitMessage 'release: improve SVG export'
```

## 脚本执行流程

脚本按以下顺序执行：

1. 检查 Git 工作区是否存在未提交修改。
2. 读取 `crates/cdr-desktop/Cargo.toml` 的当前版本。
3. 有未提交修改时按 `-Bump` 计算新版本；工作区干净时保持当前版本。
4. 有未提交修改时同步更新以下文件：
   - `crates/cdr-desktop/Cargo.toml`
   - `Cargo.lock` 中的 `cdr-desktop` 包版本
   - `crates/cdr-plugin/package/content/extension.xml`
   - `crates/cdr-plugin/package/META-INF/metadata.xml`
5. 调用 `scripts/build-portable.ps1`；工作区干净时允许覆盖当前版本的便携产物以便重建。
6. 编译 Corel 处理器、桌面程序和插件，并复制 OpenCV、VC 运行库。
7. 创建便携目录、ZIP 和 `SHA256SUMS.txt`。
8. 只有存在未提交修改时才执行 `git add -A` 和 `git commit`。
9. 只有创建了新提交时，`-Push` 才会执行 `git push`。
10. 只有创建了新版本时，`-PublishRelease` 才会使用 `gh release create` 上传发布资产。
11. 如果传入 `-CreateIssue`，使用本次 commit message 创建 New Issue，并写入 Release 链接。

脚本设置 `CARGO_BUILD_JOBS=1` 和 `CARGO_INCREMENTAL=0`，以降低 Windows 本机编译时的内存压力。

## 产物检查

比较 ZIP 实际 SHA-256 与校验文件：

```powershell
$zip = 'target\portable\CDR-ImageOutline-0.1.26-Windows-x64.zip'
(Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
Get-Content target\portable\SHA256SUMS.txt
```

检查便携包中的主要文件：

```powershell
Get-ChildItem 'target\portable\CDR-ImageOutline-0.1.26' -File |
  Select-Object Name, Length
```

预期至少包含：

- `cdr-desktop.exe`
- `cdr-corel.exe`
- `CdrOutline.CorelExtension`
- `opencv_world4140.dll`
- x64 VC 运行库 DLL
- `使用说明.txt`

查看刚刚创建的提交：

```powershell
git show --stat --oneline -1
```

## GitHub 自动更新要求

桌面程序启动时会读取项目的 GitHub Releases API，并比较程序内置版本与最新 Release 标签。自动更新只识别满足以下条件的 Release：

1. Release 标签版本高于当前程序版本，例如 `v0.1.26`。
2. Release 中有名称以 `-Windows-x64.zip` 结尾的 ZIP。
3. Release 中有名称严格为 `SHA256SUMS.txt` 的校验文件。
4. 校验文件中包含该 ZIP 的 64 位 SHA-256 值。

推荐使用：

```powershell
.\scripts\release.ps1 -Push -PublishRelease -CreateIssue
```

如果只使用 `-Push`，代码会推送到 GitHub，但桌面程序不会因此自动获得新的下载包。

### Release 与 New Issue 的关系

ZIP 和 `SHA256SUMS.txt` 作为 Release 资产上传。New Issue 不直接保存 ZIP，而是记录以下信息：

- 本次 Git commit message，作为版本说明。
- 版本号和短提交哈希。
- GitHub Release 下载链接。
- SHA-256 校验文件说明。

这样 Issue 适合作为发布记录，Release 负责保存可下载的 ZIP 资产。

## 故障排查

### `gh` 未找到

`-PublishRelease` 需要 GitHub CLI。安装后执行：

```powershell
gh auth login
gh auth status
```

然后重新执行带 `-PublishRelease` 的命令。

### 同版本便携包已存在

有代码或文档修改时，脚本不会覆盖同名目录或 ZIP。工作区干净时，脚本会使用安全的 `-Force` 路径，只覆盖 `target\portable` 下当前版本的生成目录和 ZIP，以便重建。

### OpenCV 或 VC 运行库缺失

使用 `-OpenCvRoot` 或 `-VCRedistDir` 指向实际目录。OpenCV 目录需要能找到 `opencv_world4140.dll`，VC 目录需要包含 x64 VC143 DLL。

### CorelDRAW interop 程序集找不到

使用 `-CorelInstallDir` 指向包含以下文件的 CorelDRAW 安装目录：

```text
Programs64\Assemblies\Corel.Interop.VGCore.dll
```

### 构建因内存不足失败

保持单任务构建：

```powershell
.\scripts\release.ps1 -Jobs 1
```

构建失败后，版本文件可能已经更新但尚未提交。先检查：

```powershell
git diff -- crates/cdr-desktop/Cargo.toml Cargo.lock crates/cdr-plugin/package
```

确认状态后，再决定继续构建或恢复这些版本文件。

### Git push 被拒绝

如果提交已经成功、只有推送失败，不要重新运行发布脚本，否则会再次递增版本。先处理远程分支后直接推送当前提交：

```powershell
git pull --rebase origin main
git push origin main
```

### GitHub Release 创建失败

如果代码已推送但 Release 创建失败，不要重新递增版本。先确认 `gh auth status`，再使用相同版本的 ZIP 和校验文件手动创建 Release。

### New Issue 创建失败

如果 Release 已创建但 Issue 创建失败，不要重新运行完整发布脚本。先确认 `gh auth status`，然后从 Release 页面复制链接，手动创建 Issue，并将本次 commit message 作为版本说明。

## 安全注意事项

- `-Push` 会向远程仓库写入提交；使用前请检查 `git status`。
- `-PublishRelease` 会创建公开或仓库可见的 GitHub Release，取决于仓库权限和可见性。
- 脚本不会修改 CDR 文档，也不会自动关闭 CorelDRAW。
- 新版本发布不会覆盖同版本发布目录或 ZIP；干净工作区的重建只覆盖 `target\portable` 中当前版本的生成物。
- 旧版本便携包不会被删除，便于回退和对比。
