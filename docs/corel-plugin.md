# CorelDRAW 2020 插件

项目保留两个入口，并共用 `cdr-corel` 的 Rust 处理核心：

- `cdr-desktop.exe`：独立桌面窗口；
- `CdrOutline.CorelExtension`：CorelDRAW 2020 内嵌面板。

内嵌面板使用 CorelDRAW 2020 官方扩展机制要求的轻量 .NET Framework/WPF 外壳。外壳只负责参数输入、状态显示和启动同包内的 `cdr-corel.exe`；透明边缘矢量化、巡边、孔位和写回仍由 Rust 完成。

使用时，先对位图点“巡边”；需要孔位时再点“加孔”生成独立孔曲线。最后同时选中巡边曲线和孔曲线点“合并孔位”，程序只焊接与轮廓实际相交的孔，不接触的孔保持原样。合并操作不是重新处理位图。

## 构建

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-corel-extension.ps1
```

构建结果位于：

```text
target\corel-extension\CdrOutline.CorelExtension
target\corel-extension\unpacked\
```

脚本只写入项目的 `target` 目录，不会自动安装或改动 CorelDRAW 安装目录。实际安装前应先检查扩展包内容，并保留可卸载路径。

## 安装到 CorelDRAW 2020

1. 保存所有打开的 CDR 文档并退出 CorelDRAW。
2. 将 `target\corel-extension\CdrOutline.CorelExtension` 复制到 CorelDRAW 安装目录的 `Extensions` 文件夹。示例：本机路径是

   ```text
   D:\apps\CorelDRAW Graphics Suite 2020\Extensions\CdrOutline.CorelExtension
   ```

3. 重新启动 CorelDRAW。扩展包升级后必须重启，正在运行的 CorelDRAW 2020 会继续使用旧的解包缓存。
4. 从“窗口 → 泊坞窗 → CDR 巡边与孔位”打开面板。

菜单入口使用 Corel 的本地资源表注册，泊坞窗结构由 `AppUI.xslt` 和 `UserUI.xslt` 写入 Corel 工作区。安装包必须同时包含这两个 XSLT、`CdrOutlineIntl.dll` 和 `config.xml` 中的 GUID 映射，否则可能只显示空泊坞窗或完全没有菜单入口。

构建后可先运行包结构探针，不需要启动 CorelDRAW：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-corel-plugin-package.ps1
```

CorelDRAW 运行时可用以下只读探针检查菜单注册，不会保存或修改文档：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-corel-plugin-menu.ps1
```

成功时输出 `COREL_PLUGIN_MENU=PASS`。卸载时先退出 CorelDRAW，再删除上述 `.CorelExtension` 文件；用户目录中的旧解包缓存可保留，Corel 会根据安装目录重新扫描。
