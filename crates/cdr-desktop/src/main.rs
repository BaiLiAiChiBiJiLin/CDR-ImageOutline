#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("cdr-desktop requires Windows and CorelDRAW 2020");
    std::process::exit(1);
}

#[cfg(windows)]
#[path = "deployment.rs"]
mod deployment;
#[cfg(windows)]
mod updater;

#[cfg(windows)]
mod app {
    use crate::deployment;
    use crate::updater::{self, UpdateInfo};
    use cdr_corel::SelectionOperation;
    use eframe::egui::{
        self, Color32, FontData, FontDefinitions, FontFamily, FontId, Frame, Grid, Margin,
        RichText, Spinner, Stroke, TextStyle, Vec2,
    };
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    const WINDOW_TITLE: &str = "CDR 巡边与孔位";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    struct Palette;

    impl Palette {
        const BACKGROUND: Color32 = Color32::from_rgb(248, 250, 252);
        const SURFACE: Color32 = Color32::WHITE;
        const PRIMARY: Color32 = Color32::from_rgb(37, 99, 235);
        const PRIMARY_HOVER: Color32 = Color32::from_rgb(29, 78, 216);
        const PRIMARY_ACTIVE: Color32 = Color32::from_rgb(30, 64, 175);
        const TEXT: Color32 = Color32::from_rgb(30, 41, 59);
        const MUTED_TEXT: Color32 = Color32::from_rgb(71, 85, 105);
        const BORDER: Color32 = Color32::from_rgb(203, 213, 225);
        const SUBTLE: Color32 = Color32::from_rgb(241, 245, 249);
        const SUCCESS: Color32 = Color32::from_rgb(21, 128, 61);
        const ERROR: Color32 = Color32::from_rgb(185, 28, 28);
    }

    enum RunState {
        Idle,
        Running,
        Success(String),
        Error(String),
    }

    enum WorkerMessage {
        Progress(f32, String),
        Success(String),
        PrintFlowUploadSuccess(String),
        Error(String),
    }

    pub struct CdrDesktopApp {
        outline_offset_mm: f64,
        hole_diameter_mm: f64,
        hole_edge_clearance_mm: f64,
        tool_diameter_mm: f64,
        smoothing_mm: f64,
        progress: f32,
        progress_stage: String,
        state: RunState,
        worker: Option<Receiver<WorkerMessage>>,
        bundle_dir: PathBuf,
        bundle_status: deployment::BundleStatus,
        corel_install_dir: String,
        deployment_status: String,
        deployment_worker: Option<Receiver<Result<String, String>>>,
        upload_after_export: bool,
        update_worker: Option<Receiver<Result<Option<UpdateInfo>, String>>>,
        update_info: Option<UpdateInfo>,
        update_status: String,
        update_started: bool,
    }

    impl CdrDesktopApp {
        fn new(context: &eframe::CreationContext<'_>) -> Self {
            install_chinese_font(&context.egui_ctx);
            configure_style(&context.egui_ctx);
            let bundle_dir = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            let bundle_status = deployment::inspect_bundle(&bundle_dir);
            let corel_install_dir = deployment::discover_corel_installation()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default();
            Self {
                outline_offset_mm: 2.0,
                hole_diameter_mm: 3.5,
                hole_edge_clearance_mm: 2.0,
                tool_diameter_mm: 2.0,
                smoothing_mm: 0.02,
                progress: 0.0,
                progress_stage: "等待处理".to_owned(),
                state: RunState::Idle,
                worker: None,
                bundle_dir,
                bundle_status,
                corel_install_dir,
                deployment_status: "检查运行依赖和 CorelDRAW 安装位置。".to_owned(),
                deployment_worker: None,
                upload_after_export: false,
                update_worker: None,
                update_info: None,
                update_status: format!("当前版本 v{}，正在检查更新…", updater::current_version()),
                update_started: false,
            }
        }

        fn start_update_check(&mut self, context: &egui::Context) {
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let _ = sender.send(updater::check_latest());
                repaint_context.request_repaint();
            });
            self.update_worker = Some(receiver);
            self.update_started = true;
        }

        fn start_update_install(&mut self, info: UpdateInfo, context: &egui::Context) {
            let bundle = self.bundle_dir.clone();
            let version = info.version.clone();
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let result = updater::download_and_install(&info, &bundle).map(|_| None);
                let _ = sender.send(result);
                repaint_context.request_repaint();
            });
            self.update_worker = Some(receiver);
            self.update_status = format!("正在下载 v{}…", version);
            self.update_info = None;
        }

        fn poll_update(&mut self) {
            let Some(receiver) = &self.update_worker else {
                return;
            };
            if let Ok(result) = receiver.try_recv() {
                self.update_worker = None;
                match result {
                    Ok(Some(info)) => {
                        self.update_status = format!("发现新版本 v{}", info.version);
                        self.update_info = Some(info);
                    }
                    Ok(None) => {
                        self.update_status = "更新包已下载，程序将关闭并自动完成更新。".to_owned();
                        std::process::exit(0);
                    }
                    Err(error) => self.update_status = error,
                }
            }
        }

        fn update_panel(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
            Frame::new()
                .fill(Palette::SURFACE)
                .stroke(Stroke::new(1.0_f32, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("软件更新").strong().color(Palette::TEXT));
                        ui.label(
                            RichText::new(&self.update_status)
                                .size(12.0)
                                .color(Palette::MUTED_TEXT),
                        );
                        if let Some(info) = self.update_info.clone() {
                            if ui
                                .add_enabled(
                                    self.update_worker.is_none(),
                                    egui::Button::new("下载并更新"),
                                )
                                .clicked()
                            {
                                self.start_update_install(info, context);
                            }
                        }
                        if ui
                            .add_enabled(
                                self.update_worker.is_none(),
                                egui::Button::new("检查更新"),
                            )
                            .clicked()
                        {
                            self.start_update_check(context);
                        }
                    });
                });
        }

        fn start_processing(&mut self, operation: SelectionOperation, context: &egui::Context) {
            let processor = self.bundle_dir.join("cdr-corel.exe");
            let arguments = match operation {
                SelectionOperation::Outline => vec![
                    "outline-selection".to_owned(),
                    self.outline_offset_mm.to_string(),
                    self.tool_diameter_mm.to_string(),
                    self.smoothing_mm.to_string(),
                ],
                SelectionOperation::AddHoles => vec![
                    "add-selection-holes".to_owned(),
                    self.hole_diameter_mm.to_string(),
                    self.hole_edge_clearance_mm.to_string(),
                ],
                SelectionOperation::MergeHoles => vec!["merge-selected-holes".to_owned()],
                SelectionOperation::TrimTransparent => vec![
                    "trim-transparent-selection".to_owned(),
                    self.outline_offset_mm.to_string(),
                    self.tool_diameter_mm.to_string(),
                    self.smoothing_mm.to_string(),
                ],
            };
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let progress_sender = sender.clone();
                let progress_context = repaint_context.clone();
                let result = run_processor(&processor, &arguments, move |progress, stage| {
                    let _ =
                        progress_sender.send(WorkerMessage::Progress(progress, stage.to_owned()));
                    progress_context.request_repaint();
                });
                let message = match result {
                    Ok(summary) => WorkerMessage::Success(summary),
                    Err(error) => WorkerMessage::Error(error),
                };
                let _ = sender.send(message);
                repaint_context.request_repaint();
            });
            self.worker = Some(receiver);
            self.progress = 0.0;
            self.progress_stage = "开始处理".to_owned();
            self.state = RunState::Running;
        }

        fn start_export(&mut self, selected: bool, context: &egui::Context) {
            let processor = self.bundle_dir.join("cdr-corel.exe");
            let arguments = vec![if selected {
                "export-selection-svg".to_owned()
            } else {
                "export-page-svg".to_owned()
            }];
            let auto_upload = self.upload_after_export;
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let progress_sender = sender.clone();
                let progress_context = repaint_context.clone();
                let result = run_processor(&processor, &arguments, move |progress, stage| {
                    let _ =
                        progress_sender.send(WorkerMessage::Progress(progress, stage.to_owned()));
                    progress_context.request_repaint();
                })
                .and_then(|summary| {
                    if !auto_upload {
                        return Ok((summary, false));
                    }
                    let path = export_path_from_summary(&summary)
                        .ok_or_else(|| "导出成功但未找到输出 SVG 路径".to_owned())?;
                    let upload_message = upload_to_printflow(&path)?;
                    Ok((format!("{summary}\n{upload_message}"), true))
                });
                let message = match result {
                    Ok((summary, true)) => WorkerMessage::PrintFlowUploadSuccess(summary),
                    Ok((summary, false)) => WorkerMessage::Success(summary),
                    Err(error) => WorkerMessage::Error(error),
                };
                let _ = sender.send(message);
                repaint_context.request_repaint();
            });
            self.worker = Some(receiver);
            self.progress = 0.0;
            self.progress_stage = if auto_upload {
                "导出 SVG，随后上传 PrintFlow".to_owned()
            } else {
                "正在导出 SVG".to_owned()
            };
            self.state = RunState::Running;
        }

        fn poll_worker(&mut self) {
            let Some(receiver) = &self.worker else {
                return;
            };
            let mut finished = false;
            while let Ok(message) = receiver.try_recv() {
                match message {
                    WorkerMessage::Progress(progress, stage) => {
                        self.progress = progress.clamp(0.0, 1.0);
                        self.progress_stage = stage;
                    }
                    WorkerMessage::Success(summary) => {
                        self.progress = 1.0;
                        self.progress_stage = "处理完成".to_owned();
                        self.state = RunState::Success(summary);
                        finished = true;
                    }
                    WorkerMessage::PrintFlowUploadSuccess(summary) => {
                        self.progress = 1.0;
                        self.progress_stage = "处理完成，正在切换到 PrintFlow".to_owned();
                        let summary =
                            upload_and_focus_printflow(|| Ok(summary), focus_printflow_window)
                                .expect("the PrintFlow upload was already accepted");
                        self.state = RunState::Success(summary);
                        finished = true;
                    }
                    WorkerMessage::Error(error) => {
                        self.state = RunState::Error(error);
                        finished = true;
                    }
                }
            }
            if finished {
                self.worker = None;
            }
        }

        fn outline_parameters(&mut self, ui: &mut egui::Ui, enabled: bool) {
            Frame::new()
                .fill(Palette::SURFACE)
                .stroke(Stroke::new(1.0_f32, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("巡边")
                            .size(15.0)
                            .strong()
                            .color(Palette::TEXT),
                    );
                    ui.add_space(10.0);
                    ui.add_enabled_ui(enabled, |ui| {
                        Grid::new("outline_parameters")
                            .num_columns(3)
                            .spacing(Vec2::new(16.0, 12.0))
                            .min_col_width(76.0)
                            .show(ui, |ui| {
                                parameter_row(
                                    ui,
                                    "巡边外扩",
                                    &mut self.outline_offset_mm,
                                    0.1..=50.0,
                                    0.1,
                                    1,
                                );
                                parameter_row(
                                    ui,
                                    "道具口径",
                                    &mut self.tool_diameter_mm,
                                    0.0..=100.0,
                                    0.1,
                                    1,
                                );
                                parameter_row(
                                    ui,
                                    "线条平滑",
                                    &mut self.smoothing_mm,
                                    0.0..=2.0,
                                    0.01,
                                    2,
                                );
                            });
                    });
                });
        }

        fn hole_parameters(&mut self, ui: &mut egui::Ui, enabled: bool) {
            Frame::new()
                .fill(Palette::SURFACE)
                .stroke(Stroke::new(1.0_f32, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("钥匙孔")
                            .size(15.0)
                            .strong()
                            .color(Palette::TEXT),
                    );
                    ui.add_space(10.0);
                    ui.add_enabled_ui(enabled, |ui| {
                        Grid::new("hole_parameters")
                            .num_columns(3)
                            .spacing(Vec2::new(16.0, 12.0))
                            .min_col_width(76.0)
                            .show(ui, |ui| {
                                parameter_row(
                                    ui,
                                    "孔径",
                                    &mut self.hole_diameter_mm,
                                    0.0..=100.0,
                                    0.1,
                                    1,
                                );
                                parameter_row(
                                    ui,
                                    "孔边距",
                                    &mut self.hole_edge_clearance_mm,
                                    0.1..=100.0,
                                    0.1,
                                    1,
                                );
                            });
                    });
                });
        }

        fn status(&self, ui: &mut egui::Ui) {
            let border = if matches!(self.state, RunState::Error(_)) {
                Palette::ERROR
            } else {
                Palette::BORDER
            };
            Frame::new()
                .fill(Palette::SURFACE)
                .stroke(Stroke::new(1.0_f32, border))
                .corner_radius(8)
                .inner_margin(Margin::same(18))
                .show(ui, |ui| {
                    ui.set_min_height(260.0);
                    ui.label(
                        RichText::new("处理状态")
                            .size(16.0)
                            .strong()
                            .color(Palette::TEXT),
                    );
                    ui.add_space(14.0);
                    match &self.state {
                        RunState::Idle => {
                            ui.label(
                                RichText::new("等待处理")
                                    .strong()
                                    .color(Palette::MUTED_TEXT),
                            );
                            ui.label(
                                RichText::new("在 CorelDRAW 中选择位图后开始。")
                                    .color(Palette::MUTED_TEXT),
                            );
                        }
                        RunState::Running => {
                            ui.add(
                                egui::ProgressBar::new(self.progress)
                                    .show_percentage()
                                    .text(self.progress_stage.clone()),
                            );
                            ui.add_space(8.0);
                            ui.add(Spinner::new().size(18.0).color(Palette::PRIMARY));
                            ui.label(
                                RichText::new("CorelDRAW 处理期间请保持当前文档打开。")
                                    .color(Palette::MUTED_TEXT),
                            );
                        }
                        RunState::Success(summary) => {
                            ui.label(RichText::new("处理完成").strong().color(Palette::SUCCESS));
                            ui.add(
                                egui::Label::new(RichText::new(summary).color(Palette::MUTED_TEXT))
                                    .wrap(),
                            );
                        }
                        RunState::Error(error) => {
                            ui.label(RichText::new("处理失败").strong().color(Palette::ERROR));
                            ui.add(
                                egui::Label::new(RichText::new(error).color(Palette::TEXT)).wrap(),
                            );
                        }
                    }
                });
        }

        fn poll_deployment(&mut self) {
            let Some(receiver) = &self.deployment_worker else {
                return;
            };
            if let Ok(result) = receiver.try_recv() {
                self.deployment_status = match result {
                    Ok(message) => message,
                    Err(error) => format!("操作失败：{error}"),
                };
                self.deployment_worker = None;
            }
        }

        fn start_deployment(&mut self, install: bool, context: &egui::Context) {
            let corel_root = PathBuf::from(self.corel_install_dir.trim());
            let bundle = self.bundle_dir.clone();
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let result = if install {
                    deployment::install_extension(&bundle, &corel_root)
                } else {
                    deployment::uninstall_extension(&corel_root)
                };
                let _ = sender.send(result);
                repaint_context.request_repaint();
            });
            self.deployment_worker = Some(receiver);
            self.deployment_status = if install {
                "正在安装插件；不会关闭 CorelDRAW。".to_owned()
            } else {
                "正在移出插件并保留可恢复备份。".to_owned()
            };
        }

        fn deployment_panel(&mut self, ui: &mut egui::Ui, running: bool, context: &egui::Context) {
            Frame::new()
                .fill(Palette::SURFACE)
                .stroke(Stroke::new(1.0_f32, Palette::BORDER))
                .corner_radius(6)
                .inner_margin(Margin::same(16))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("运行依赖与 Corel 插件")
                            .size(15.0)
                            .strong()
                            .color(Palette::TEXT),
                    );
                    ui.add_space(8.0);
                    if self.bundle_status.is_ready() {
                        ui.label(
                            RichText::new(
                                "运行包完整：处理器、OpenCV、VC 运行库和插件包均已就绪。",
                            )
                            .color(Palette::SUCCESS),
                        );
                    } else {
                        ui.label(
                            RichText::new(if self.bundle_status.processing_ready() {
                                "图像处理依赖已就绪；插件包状态如下："
                            } else {
                                "运行包不完整，处理功能暂不可用："
                            })
                            .color(
                                if self.bundle_status.processing_ready() {
                                    Palette::MUTED_TEXT
                                } else {
                                    Palette::ERROR
                                },
                            ),
                        );
                        ui.label(
                            RichText::new(self.bundle_status.missing_files.join("、"))
                                .color(Palette::ERROR),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new("CorelDRAW 2020 安装目录").color(Palette::TEXT));
                    let corel_root = Path::new(self.corel_install_dir.trim());
                    if deployment::is_corel_installation(corel_root) {
                        let (message, color) = if deployment::has_installed_extension(corel_root) {
                            ("此目录已存在插件；更新会先备份旧版本。", Palette::SUCCESS)
                        } else {
                            ("此目录尚未安装本插件。", Palette::MUTED_TEXT)
                        };
                        ui.label(RichText::new(message).size(12.0).color(color));
                    }
                    ui.horizontal(|ui| {
                        ui.add_enabled(
                            !running && self.deployment_worker.is_none(),
                            egui::TextEdit::singleline(&mut self.corel_install_dir)
                                .desired_width(ui.available_width() - 92.0),
                        );
                        if ui
                            .add_enabled(
                                !running && self.deployment_worker.is_none(),
                                egui::Button::new("浏览…"),
                            )
                            .clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_folder()
                        {
                            self.corel_install_dir = path.to_string_lossy().into_owned();
                        }
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let can_install = !running
                            && self.deployment_worker.is_none()
                            && self.bundle_status.is_ready();
                        if action_button(
                            ui,
                            "安装 / 更新 Corel 插件",
                            can_install,
                            true,
                            (ui.available_width() - 8.0) / 2.0,
                        ) {
                            self.start_deployment(true, context);
                        }
                        if action_button(
                            ui,
                            "卸载插件（可恢复）",
                            !running && self.deployment_worker.is_none(),
                            false,
                            ui.available_width(),
                        ) {
                            self.start_deployment(false, context);
                        }
                    });
                    let deployment_status_display =
                        deployment_status_display_text(&self.deployment_status);
                    ui.add(
                        egui::Label::new(
                            RichText::new(deployment_status_display)
                                .size(12.0)
                                .color(Palette::MUTED_TEXT),
                        )
                        .wrap(),
                    )
                    .on_hover_text(&self.deployment_status);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "若 CorelDRAW 安装在受保护目录（如 Program Files）且提示拒绝访问，请关闭后以管理员身份运行本程序再安装。",
                            )
                            .size(11.0)
                            .color(Palette::MUTED_TEXT),
                        )
                        .wrap(),
                    );
                });
        }
    }

    impl eframe::App for CdrDesktopApp {
        fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
            self.poll_worker();
            self.poll_deployment();
            self.poll_update();
            if !self.update_started {
                self.start_update_check(context);
            }
            let running = matches!(self.state, RunState::Running);
            if running || self.deployment_worker.is_some() {
                context.request_repaint_after(Duration::from_millis(80));
            }

            egui::CentralPanel::default()
                .frame(
                    Frame::new()
                        .fill(Palette::BACKGROUND)
                        .inner_margin(Margin::same(24)),
                )
                .show(context, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new("CDR 巡边与孔位")
                                    .size(22.0)
                                    .strong()
                                    .color(Palette::TEXT),
                            );
                            ui.label(
                                RichText::new("CorelDRAW 2020 · 当前选择")
                                    .size(13.0)
                                    .color(Palette::MUTED_TEXT),
                            );
                            ui.add_space(20.0);
                            self.update_panel(ui, context);
                            ui.add_space(12.0);

                            let processing_ready = self.bundle_status.processing_ready();
                            ui.columns(2, |columns| {
                                let controls = &mut columns[0];
                                self.outline_parameters(controls, !running && processing_ready);
                                controls.add_space(8.0);
                                let full_width = controls.available_width();
                                if action_button(
                                    controls,
                                    "巡边",
                                    !running && processing_ready,
                                    true,
                                    full_width,
                                ) {
                                    self.start_processing(SelectionOperation::Outline, context);
                                }
                                controls.add_space(8.0);
                                if action_button(
                                    controls,
                                    "去图片透明边并等比缩放",
                                    !running && processing_ready,
                                    false,
                                    full_width,
                                ) {
                                    self.start_processing(
                                        SelectionOperation::TrimTransparent,
                                        context,
                                    );
                                }
                                controls.label(
                                    RichText::new(
                                        "裁切透明边、清理毛边，并生成贴边的细矢量曲线。图片仍是位图，画面放大后的清晰度受原图分辨率影响。",
                                    )
                                    .size(12.0)
                                    .color(Palette::MUTED_TEXT),
                                );
                                controls.add_space(16.0);
                                self.hole_parameters(controls, !running && processing_ready);
                                controls.add_space(8.0);
                                controls.horizontal(|ui| {
                                    let button_width = (ui.available_width() - 8.0) / 2.0;
                                    if action_button(
                                        ui,
                                        "加孔",
                                        !running && processing_ready,
                                        false,
                                        button_width,
                                    ) {
                                        self.start_processing(
                                            SelectionOperation::AddHoles,
                                            context,
                                        );
                                    }
                                    if action_button(
                                        ui,
                                        "合并孔位",
                                        !running && processing_ready,
                                        true,
                                        button_width,
                                    ) {
                                        self.start_processing(
                                            SelectionOperation::MergeHoles,
                                            context,
                                        );
                                    }
                                });
                                controls.label(
                                    RichText::new(
                                        "合并孔位：选择巡边曲线和孔位；只有接触轮廓的孔会融合，框选混入图片时会自动忽略图片。",
                                    )
                                    .size(12.0)
                                    .color(Palette::MUTED_TEXT),
                                );
                                controls.add_space(16.0);
                                Frame::new()
                                    .fill(Palette::SURFACE)
                                    .stroke(Stroke::new(1.0_f32, Palette::BORDER))
                                    .corner_radius(6)
                                    .inner_margin(Margin::same(16))
                                    .show(controls, |ui| {
                                        ui.checkbox(
                                            &mut self.upload_after_export,
                                            "导出完成后自动上传 PrintFlow",
                                        );
                                        ui.horizontal(|ui| {
                                            let button_width = (ui.available_width() - 8.0) / 2.0;
                                            if compact_action_button(
                                                ui,
                                                "导出选中为 SVG",
                                                !running && processing_ready,
                                                false,
                                                button_width,
                                            ) {
                                                self.start_export(true, context);
                                            }
                                            if compact_action_button(
                                                ui,
                                                "导出页面为 SVG",
                                                !running && processing_ready,
                                                true,
                                                button_width,
                                            ) {
                                                self.start_export(false, context);
                                            }
                                        });
                                    });
                            self.deployment_panel(&mut columns[1], running, context);
                            columns[1].add_space(12.0);
                            self.status(&mut columns[1]);
                            });
                        });
                });
        }
    }

    fn run_processor(
        processor: &Path,
        arguments: &[String],
        mut on_progress: impl FnMut(f32, &str),
    ) -> Result<String, String> {
        let mut child = Command::new(processor)
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!(
                    "无法启动处理器 {}：{error}。请解压完整发布包，并确认 VC 运行库文件未被杀毒软件隔离。",
                    processor.display()
                )
            })?;
        let stdout = child.stdout.take().ok_or("无法读取处理进度")?;
        let stderr = child.stderr.take().ok_or("无法读取处理错误")?;
        let stderr_worker = std::thread::spawn(move || {
            let mut output = String::new();
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                output.push_str(&line);
                output.push('\n');
            }
            output
        });

        let mut summary = String::new();
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(progress) = line.strip_prefix("__CDR_PROGRESS__\t") {
                let mut fields = progress.splitn(2, '\t');
                if let (Some(value), Some(stage)) = (fields.next(), fields.next())
                    && let Ok(value) = value.parse::<f32>()
                {
                    on_progress(value, stage);
                }
            } else if !line.trim().is_empty() {
                summary.push_str(&line);
                summary.push('\n');
            }
        }
        let status = child
            .wait()
            .map_err(|error| format!("等待处理器结束失败：{error}"))?;
        let stderr = stderr_worker.join().unwrap_or_default();
        if !status.success() {
            let detail = if stderr.trim().is_empty() {
                summary.trim()
            } else {
                stderr.trim()
            };
            return Err(format!(
                "处理失败（退出码 {:?}）：{}",
                status.code(),
                detail
            ));
        }
        Ok(summary.trim().to_owned())
    }

    fn export_path_from_summary(summary: &str) -> Option<PathBuf> {
        summary.lines().find_map(|line| {
            line.strip_prefix("__CDR_EXPORT__\t")
                .map(|path| PathBuf::from(path.trim()))
                .filter(|path| path.is_absolute() && path.extension().is_some())
        })
    }

    fn upload_to_printflow(path: &Path) -> Result<String, String> {
        if !path.is_absolute() || !path.is_file() {
            return Err(format!("SVG 文件不存在：{}", path.display()));
        }
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("svg"))
            != Some(true)
        {
            return Err("PrintFlow 只接受 .svg 文件".to_owned());
        }
        let (host, port) = discover_printflow_endpoint()?;
        let escaped = json_escape(&path.to_string_lossy());
        let body = format!(r#"{{"path":"{escaped}"}}"#);
        let response = http_request(
            &host,
            port,
            "POST",
            "/api/import-svg",
            Some(body.as_bytes()),
        )?;
        if response.status < 200 || response.status >= 300 {
            return Err(format!(
                "PrintFlow 上传失败（HTTP {}）：{}",
                response.status, response.body
            ));
        }
        Ok(format!("已提交 PrintFlow：{}", path.display()))
    }

    fn upload_and_focus_printflow<U, F>(upload: U, focus: F) -> Result<String, String>
    where
        U: FnOnce() -> Result<String, String>,
        F: FnOnce() -> bool,
    {
        let mut summary = upload()?;
        let focus_status = if focus() {
            "PrintFlow 窗口已置前。"
        } else {
            "已提交 PrintFlow，但未能自动切换窗口；请手动切换到 PrintFlow。"
        };
        summary.push('\n');
        summary.push_str(focus_status);
        Ok(summary)
    }

    fn focus_printflow_window() -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow,
        };
        use windows::core::w;

        let Ok(window) = (unsafe { FindWindowW(None, w!("PrintFlow")) }) else {
            return false;
        };
        unsafe {
            let _ = ShowWindow(window, SW_RESTORE);
            SetForegroundWindow(window).as_bool()
        }
    }

    fn discover_printflow_endpoint() -> Result<(String, u16), String> {
        let cache_path = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|root| root.join("printflow-data/cache/local-api.json"));
        if let Some(path) = cache_path.filter(|path| path.is_file())
            && let Ok(contents) = fs::read_to_string(path)
            && let Some((host, port)) = parse_printflow_endpoint(&contents)
            && (host == "127.0.0.1" || host == "localhost")
            && http_request(&host, port, "GET", "/api/health", None)
                .map(|response| response.status == 200)
                .unwrap_or(false)
        {
            return Ok((host, port));
        }
        for port in 47821..=47840 {
            if http_request("127.0.0.1", port, "GET", "/api/health", None)
                .map(|response| response.status == 200)
                .unwrap_or(false)
            {
                return Ok(("127.0.0.1".to_owned(), port));
            }
        }
        Err("未找到 PrintFlow 本地 API；请先启动 PrintFlow".to_owned())
    }

    fn parse_printflow_endpoint(json: &str) -> Option<(String, u16)> {
        let host = json_string_field(json, "host")?;
        let port = json_number_field(json, "port")?;
        Some((host, port))
    }

    fn json_string_field(json: &str, key: &str) -> Option<String> {
        let marker = format!(r#""{key}""#);
        let start = json.find(&marker)? + marker.len();
        let value = json[start..].trim_start().strip_prefix(':')?.trim_start();
        let value = value.strip_prefix('"')?;
        let end = value.find('"')?;
        Some(value[..end].to_owned())
    }

    fn json_number_field(json: &str, key: &str) -> Option<u16> {
        let marker = format!(r#""{key}""#);
        let start = json.find(&marker)? + marker.len();
        let value = json[start..].trim_start().strip_prefix(':')?.trim_start();
        let end = value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len());
        value[..end].parse().ok()
    }

    fn json_escape(value: &str) -> String {
        value
            .chars()
            .flat_map(|character| match character {
                '\\' => "\\\\".chars().collect::<Vec<_>>(),
                '"' => "\\\"".chars().collect::<Vec<_>>(),
                '\n' => "\\n".chars().collect::<Vec<_>>(),
                '\r' => "\\r".chars().collect::<Vec<_>>(),
                '\t' => "\\t".chars().collect::<Vec<_>>(),
                other => vec![other],
            })
            .collect()
    }

    struct HttpResponse {
        status: u16,
        body: String,
    }

    fn http_request(
        host: &str,
        port: u16,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, String> {
        let mut stream = TcpStream::connect((host, port))
            .map_err(|error| format!("无法连接 PrintFlow {host}:{port}：{error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("设置 PrintFlow 超时失败：{error}"))?;
        let payload = body.unwrap_or_default();
        let content_type = if body.is_some() {
            "Content-Type: application/json\r\n"
        } else {
            ""
        };
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n{content_type}Content-Length: {}\r\n\r\n",
            payload.len()
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.write_all(payload))
            .map_err(|error| format!("发送 PrintFlow 请求失败：{error}"))?;
        let mut bytes = Vec::new();
        stream
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取 PrintFlow 响应失败：{error}"))?;
        let response = String::from_utf8_lossy(&bytes);
        let mut lines = response.splitn(2, "\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| "PrintFlow 返回了无效 HTTP 响应".to_owned())?;
        let body = lines
            .next()
            .and_then(|value| value.split_once("\r\n\r\n"))
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        Ok(HttpResponse { status, body })
    }

    fn action_button(
        ui: &mut egui::Ui,
        label: &str,
        enabled: bool,
        primary: bool,
        width: f32,
    ) -> bool {
        action_button_with_size(ui, label, enabled, primary, width, None)
    }

    fn compact_action_button(
        ui: &mut egui::Ui,
        label: &str,
        enabled: bool,
        primary: bool,
        width: f32,
    ) -> bool {
        action_button_with_size(ui, label, enabled, primary, width, Some(12.0))
    }

    fn action_button_with_size(
        ui: &mut egui::Ui,
        label: &str,
        enabled: bool,
        primary: bool,
        width: f32,
        font_size: Option<f32>,
    ) -> bool {
        let foreground = if primary {
            Color32::WHITE
        } else {
            Palette::TEXT
        };
        let fill = if primary {
            Palette::PRIMARY
        } else {
            Palette::SUBTLE
        };
        let border = if primary {
            Palette::PRIMARY
        } else {
            Palette::BORDER
        };
        let mut text = RichText::new(label).strong().color(foreground);
        if let Some(size) = font_size {
            text = text.size(size);
        }
        ui.add_enabled(
            enabled,
            egui::Button::new(text)
                .fill(fill)
                .stroke(Stroke::new(1.0_f32, border))
                .min_size(Vec2::new(width, 42.0)),
        )
        .clicked()
    }

    fn parameter_row(
        ui: &mut egui::Ui,
        label: &str,
        value: &mut f64,
        range: std::ops::RangeInclusive<f64>,
        speed: f64,
        decimals: usize,
    ) {
        ui.label(RichText::new(label).color(Palette::TEXT));
        ui.add_sized(
            [220.0, 34.0],
            egui::DragValue::new(value)
                .range(range)
                .speed(speed)
                .fixed_decimals(decimals),
        );
        ui.label(RichText::new("mm").color(Palette::MUTED_TEXT));
        ui.end_row();
    }

    fn install_chinese_font(context: &egui::Context) {
        let candidates = [
            Path::new(r"C:\Windows\Fonts\msyh.ttc"),
            Path::new(r"C:\Windows\Fonts\msyh.ttf"),
            Path::new(r"C:\Windows\Fonts\simhei.ttf"),
        ];
        let Some(bytes) = candidates.iter().find_map(|path| fs::read(path).ok()) else {
            return;
        };
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert(
            "system_chinese".to_owned(),
            FontData::from_owned(bytes).into(),
        );
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "system_chinese".to_owned());
        }
        context.set_fonts(fonts);
    }

    fn configure_style(context: &egui::Context) {
        let mut style = (*context.style()).clone();
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(16.0, 10.0);
        style
            .text_styles
            .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        );
        style.text_styles.insert(
            TextStyle::Small,
            FontId::new(12.0, FontFamily::Proportional),
        );
        let visuals = &mut style.visuals;
        visuals.dark_mode = false;
        visuals.panel_fill = Palette::BACKGROUND;
        visuals.window_fill = Palette::SURFACE;
        visuals.override_text_color = Some(Palette::TEXT);
        visuals.selection.bg_fill = Palette::PRIMARY;
        visuals.selection.stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.widgets.inactive.bg_fill = Palette::SURFACE;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Palette::BORDER);
        visuals.widgets.hovered.bg_fill = Palette::PRIMARY_HOVER;
        visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.widgets.active.bg_fill = Palette::PRIMARY_ACTIVE;
        visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        visuals.widgets.noninteractive.bg_fill = Palette::SUBTLE;
        visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, Palette::BORDER);
        context.set_style(style);
    }

    fn deployment_status_display_text(status: &str) -> String {
        if status.starts_with("插件已安装到 ") {
            if status.contains("旧版本已备份到 ") {
                "插件安装完成；旧版本已自动备份。请重启 CorelDRAW 后使用。".to_owned()
            } else {
                "插件安装完成。请重启 CorelDRAW 后使用。".to_owned()
            }
        } else {
            status.to_owned()
        }
    }

    pub fn run() -> eframe::Result {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title(WINDOW_TITLE)
                .with_inner_size([1040.0, 780.0])
                .with_min_inner_size([880.0, 640.0]),
            ..Default::default()
        };
        eframe::run_native(
            WINDOW_TITLE,
            options,
            Box::new(|context| Ok(Box::new(CdrDesktopApp::new(context)))),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::{deployment_status_display_text, upload_and_focus_printflow};

        #[test]
        fn successful_printflow_upload_requests_window_focus() {
            let mut focus_requested = false;

            let result = upload_and_focus_printflow(
                || Ok("已提交 PrintFlow".to_owned()),
                || {
                    focus_requested = true;
                    true
                },
            )
            .expect("a successful upload should remain successful");

            assert!(
                focus_requested,
                "PrintFlow should be brought to the foreground"
            );
            assert!(result.contains("PrintFlow 窗口已置前"));
        }

        #[test]
        fn summarizes_install_status_without_inline_paths() {
            let status = "插件已安装到 D:\\apps\\CorelDRAW Graphics Suite 2020\\Extensions\\CdrOutline.CorelExtension。请重启 CorelDRAW 后使用。旧版本已备份到 D:\\apps\\CorelDRAW Graphics Suite 2020\\Extensions\\_CdrOutlineBackups\\backup-id\\CdrOutline.CorelExtension。";
            let display = deployment_status_display_text(status);

            assert!(!display.contains("D:\\apps"));
            assert!(display.contains("插件安装完成"));
            assert!(display.contains("旧版本已自动备份"));
        }
    }
}

#[cfg(windows)]
fn main() -> eframe::Result {
    app::run()
}
