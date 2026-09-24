# CDR 巡边与孔位工具（制作亚克力产品等）

clean-room Rust 项目。

## 当前状态
- 已建立 CDR 位图只读导出和 Rust alpha 预检流程，可区分真实透明图片与不透明白底图。
- 已能为真实透明图片生成 Rust `2 mm` 巡边 SVG 预览。
- 已实现默认顶部中心孔位、承托圆合并和 `3 mm` 内孔扣除的复合 SVG 预览。
- 已实现测试级 Rust CorelDRAW COM 写回：只复制并修改输出副本，在 `RUST_TEST` 图层创建复合曲线。
- 已支持读取 CorelDRAW 当前选择中的位图并直接生成巡边与孔位，结果写入唯一的 `RUST_OUTPUT` 图层。
- 当前选择写回不自动保存文档，并归入单个命令组，可一次撤销。
- 已提供独立桌面程序和 CorelDRAW 2020 泊坞窗插件；两者共用同一 Rust 处理核心。
- 已支持 `线条平滑` 参数（默认 `0.02 mm`，`0` 表示保留全部细节）。
- 已支持 `刀具口径` 参数（默认 `2 mm`）；深窄凹槽宽度小于该值时自动闭合，避免刀具无法进入。
- 尚未实现旋转、裁剪或镜像位图以及生产级批处理。


## 本地验证

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

在 CorelDRAW 中选择一张或多张未旋转、未裁剪的位图后，可运行：

```powershell
cargo run -p cdr-corel -- process-selection target\selection-run\selection-report.json
```

命令不会自动保存活动文档。没有真实透明边缘的位图会被跳过，非位图选择会在修改文档前报错。

