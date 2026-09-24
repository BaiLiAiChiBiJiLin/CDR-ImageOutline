using Corel.Interop.VGCore;
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Text;
using System.Net;
using System.Text.RegularExpressions;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Controls;
using System.Windows.Media;
using WpfGrid = System.Windows.Controls.Grid;

[assembly: AssemblyTitle("CDR 巡边与孔位")]
[assembly: AssemblyDescription("CorelDRAW 2020 panel for the Rust outline processor")]
[assembly: AssemblyCompany("CDR Outline Tool")]
[assembly: AssemblyProduct("CDR 巡边与孔位")]
[assembly: AssemblyVersion("0.1.25.0")]
[assembly: AssemblyFileVersion("0.1.25.0")]

namespace CGS
{
    public sealed class Addon
    {
        public Addon(ICUIApplication application)
        {
            try
            {
                application.FrameWork.RemoveDocker(AddonDataSource.LegacyDockerGuid);
            }
            catch (COMException)
            {
                // The legacy docker only exists after the old manual registration.
            }

            application.RegisterDataSource(
                AddonDataSource.DataSourceName,
                new AddonDatasourceFactory(),
                null,
                false);
        }
    }

    public sealed class AddonDatasourceFactory : ICUIDataSourceFactory
    {
        public object CreateDataSource(string dataSourceName, DataSourceProxy proxy)
        {
            return new AddonDataSource(proxy);
        }
    }

    [ComVisible(true)]
    [ClassInterface(ClassInterfaceType.AutoDual)]
    public sealed class AddonDataSource : INotifyPropertyChanged
    {
        public const string DataSourceName = "CdrOutlineDatasource";
        public const string DockerGuid = "6d7439bb-deb6-4784-8f31-5a6a25413fd7";
        public const string LegacyDockerGuid = "a4bd271c-9bb9-455e-b4a6-276478cf13ef";

        private readonly DataSourceProxy proxy;

        public AddonDataSource(DataSourceProxy proxy)
        {
            this.proxy = proxy;
        }

        public event PropertyChangedEventHandler PropertyChanged;

        public string DialogContent
        {
            get
            {
                return Assembly.GetExecutingAssembly().Location
                    + ", CdrOutlinePlugin.OutlinePanel";
            }
        }

        public void OnShowDialog()
        {
            proxy.Application.FrameWork.ShowDocker(DockerGuid);
        }

        public void OnHideDialog()
        {
            proxy.Application.FrameWork.HideDocker(DockerGuid);
        }

        public void NotifyPropertyChanged(string propertyName)
        {
            var handler = PropertyChanged;
            if (handler != null)
            {
                handler(this, new PropertyChangedEventArgs(propertyName));
            }
        }
    }
}

namespace CdrOutlinePlugin
{
    public sealed class OutlinePanel : UserControl
    {
        private const int SwRestore = 9;

        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr FindWindow(string className, string windowName);

        [DllImport("user32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool ShowWindow(IntPtr window, int command);

        [DllImport("user32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool SetForegroundWindow(IntPtr window);

        private static readonly Brush BackgroundBrush = BrushFrom("#F8FAFC");
        private static readonly Brush SurfaceBrush = BrushFrom("#FFFFFF");
        private static readonly Brush TextBrush = BrushFrom("#0F172A");
        private static readonly Brush MutedBrush = BrushFrom("#475569");
        private static readonly Brush BorderColorBrush = BrushFrom("#CBD5E1");
        private static readonly Brush PrimaryBrush = BrushFrom("#2563EB");
        private static readonly Brush SuccessBrush = BrushFrom("#15803D");
        private static readonly Brush ErrorBrush = BrushFrom("#B91C1C");

        private readonly TextBox outlineOffset;
        private readonly TextBox holeDiameter;
        private readonly TextBox holeEdgeClearance;
        private readonly TextBox toolDiameter;
        private readonly TextBox smoothing;
        private readonly Button outlineButton;
        private readonly Button trimButton;
        private readonly Button addHoleButton;
        private readonly Button mergeHolesButton;
        private readonly Button exportSelectionButton;
        private readonly Button exportPageButton;
        private readonly CheckBox autoUploadPrintFlow;
        private readonly ProgressBar progressBar;
        private readonly TextBlock statusText;

        public OutlinePanel(object context)
        {
            DataContext = context;
            FontFamily = new FontFamily("Microsoft YaHei UI");
            FontSize = 13;
            Background = BackgroundBrush;

            outlineOffset = CreateNumberBox("2.0", "巡边外扩，单位毫米");
            holeDiameter = CreateNumberBox("3.5", "孔径，单位毫米，0 表示不生成孔位");
            holeEdgeClearance = CreateNumberBox("2.0", "孔边距，单位毫米");
            toolDiameter = CreateNumberBox("2.0", "刀具口径，单位毫米，0 表示不做窄槽闭合");
            smoothing = CreateNumberBox("0.02", "线条平滑容差，单位毫米，0 表示保留全部细节");

            outlineButton = CreateActionButton("巡边", true);
            trimButton = CreateActionButton("去图片透明边并等比缩放", false);
            addHoleButton = CreateActionButton("加孔", false);
            mergeHolesButton = CreateActionButton("合并孔位", true);
            exportSelectionButton = CreateActionButton("导出选中为 SVG", false);
            exportPageButton = CreateActionButton("导出页面为 SVG", true);
            autoUploadPrintFlow = new CheckBox
            {
                Content = "导出完成后自动上传 PrintFlow",
                Foreground = TextBrush,
                Margin = new Thickness(0, 0, 0, 8),
                IsChecked = false
            };
            outlineButton.Click += async delegate { await ProcessSelectionAsync("outline"); };
            trimButton.Click += async delegate { await ProcessSelectionAsync("trim"); };
            addHoleButton.Click += async delegate { await ProcessSelectionAsync("add-holes"); };
            mergeHolesButton.Click += async delegate { await ProcessSelectionAsync("merge-holes"); };
            exportSelectionButton.Click += async delegate { await ExportSvgAsync("export-selection"); };
            exportPageButton.Click += async delegate { await ExportSvgAsync("export-page"); };

            progressBar = new ProgressBar
            {
                Minimum = 0,
                Maximum = 100,
                Height = 8,
                IsIndeterminate = false
            };

            statusText = new TextBlock
            {
                Text = "在 CorelDRAW 中选择位图后开始。",
                Foreground = MutedBrush,
                FontSize = 12,
                Margin = new Thickness(0, 5, 0, 0),
                TextWrapping = TextWrapping.Wrap,
                TextTrimming = TextTrimming.CharacterEllipsis,
                MaxHeight = 66,
                MinHeight = 30
            };
            statusText.ToolTip = statusText.Text;
            AutomationProperties.SetLiveSetting(statusText, AutomationLiveSetting.Polite);

            Content = BuildLayout();
        }

        private UIElement BuildLayout()
        {
            var root = new WpfGrid
            {
                Margin = new Thickness(8),
                Background = BackgroundBrush
            };
            root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
            root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });

            var title = new TextBlock
            {
                Text = "CDR 巡边与孔位",
                FontSize = 18,
                FontWeight = FontWeights.SemiBold,
                Foreground = TextBrush,
                Margin = new Thickness(0, 0, 0, 7)
            };
            WpfGrid.SetRow(title, 0);
            root.Children.Add(title);

            var content = new StackPanel();
            content.Children.Add(trimButton);
            var outlineSection = new StackPanel();
            outlineSection.Children.Add(CreateParameterRow("巡边外扩", outlineOffset));
            outlineSection.Children.Add(CreateParameterRow("道具口径", toolDiameter));
            outlineSection.Children.Add(CreateParameterRow("线条平滑", smoothing));
            outlineSection.Children.Add(outlineButton);
            content.Children.Add(CreateSection("巡边", outlineSection));

            var holeSection = new StackPanel();
            holeSection.Children.Add(CreateParameterRow("孔径", holeDiameter));
            holeSection.Children.Add(CreateParameterRow("孔边距", holeEdgeClearance));
            var holeActions = new WpfGrid { Margin = new Thickness(0, 2, 0, 0) };
            holeActions.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            holeActions.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            addHoleButton.Margin = new Thickness(0, 0, 4, 0);
            mergeHolesButton.Margin = new Thickness(4, 0, 0, 0);
            WpfGrid.SetColumn(addHoleButton, 0);
            WpfGrid.SetColumn(mergeHolesButton, 1);
            holeActions.Children.Add(addHoleButton);
            holeActions.Children.Add(mergeHolesButton);
            holeSection.Children.Add(holeActions);
            holeSection.Children.Add(new TextBlock
            {
                Text = "选择巡边曲线和孔位；只有实际接触轮廓的孔会融合。",
                Foreground = MutedBrush,
                FontSize = 11,
                Margin = new Thickness(0, 6, 0, 0),
                TextWrapping = TextWrapping.Wrap
            });
            content.Children.Add(CreateSection("钥匙孔", holeSection));

            var exportSection = new StackPanel();
            exportSection.Children.Add(new TextBlock
            {
                Text = "默认导出到桌面；同名 SVG 会直接替换。",
                Foreground = MutedBrush,
                FontSize = 11,
                Margin = new Thickness(0, 0, 0, 6),
                TextWrapping = TextWrapping.Wrap
            });
            exportSection.Children.Add(autoUploadPrintFlow);
            var exportActions = new WpfGrid { Margin = new Thickness(0, 2, 0, 0) };
            exportActions.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            exportActions.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            exportSelectionButton.Margin = new Thickness(0, 0, 4, 0);
            exportPageButton.Margin = new Thickness(4, 0, 0, 0);
            WpfGrid.SetColumn(exportSelectionButton, 0);
            WpfGrid.SetColumn(exportPageButton, 1);
            exportActions.Children.Add(exportSelectionButton);
            exportActions.Children.Add(exportPageButton);
            exportSection.Children.Add(exportActions);
            content.Children.Add(CreateSection("透明 SVG", exportSection));

            var scroll = new ScrollViewer
            {
                VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
            Content = content
        };
            WpfGrid.SetRow(scroll, 1);
            root.Children.Add(scroll);

            var statusContent = new StackPanel();
            statusContent.Children.Add(progressBar);
            statusContent.Children.Add(statusText);
            var statusFooter = new Border
            {
                Background = SurfaceBrush,
                BorderBrush = BorderColorBrush,
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(4),
                Padding = new Thickness(8, 6, 8, 7),
                Margin = new Thickness(0, 7, 0, 0),
                Child = statusContent
            };
            WpfGrid.SetRow(statusFooter, 2);
            root.Children.Add(statusFooter);
            return root;
        }

        private static Border CreateSection(string title, UIElement body)
        {
            var section = new StackPanel();
            section.Children.Add(new TextBlock
            {
                Text = title,
                FontWeight = FontWeights.SemiBold,
                Foreground = TextBrush,
                Margin = new Thickness(0, 0, 0, 6)
            });
            section.Children.Add(body);
            return new Border
            {
                Background = SurfaceBrush,
                BorderBrush = BorderColorBrush,
                BorderThickness = new Thickness(1),
                CornerRadius = new CornerRadius(4),
                Padding = new Thickness(10),
                Margin = new Thickness(0, 0, 0, 7),
                Child = section
            };
        }

        private static WpfGrid CreateParameterRow(string label, TextBox input)
        {
            var row = new WpfGrid { Margin = new Thickness(0, 0, 0, 6) };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(78) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(28) });

            var text = new TextBlock
            {
                Text = label,
                Foreground = TextBrush,
                VerticalAlignment = VerticalAlignment.Center
            };
            WpfGrid.SetColumn(text, 0);
            row.Children.Add(text);

            WpfGrid.SetColumn(input, 1);
            row.Children.Add(input);

            var unit = new TextBlock
            {
                Text = "mm",
                Foreground = MutedBrush,
                Margin = new Thickness(6, 0, 0, 0),
                VerticalAlignment = VerticalAlignment.Center
            };
            WpfGrid.SetColumn(unit, 2);
            row.Children.Add(unit);
            return row;
        }

        private static TextBox CreateNumberBox(string value, string automationName)
        {
            var input = new TextBox
            {
                Text = value,
                MinHeight = 30,
                Padding = new Thickness(7, 4, 7, 4),
                Foreground = TextBrush,
                Background = SurfaceBrush,
                BorderBrush = BorderColorBrush,
                BorderThickness = new Thickness(1),
                VerticalContentAlignment = VerticalAlignment.Center
            };
            AutomationProperties.SetName(input, automationName);
            return input;
        }

        private static Button CreateActionButton(string label, bool primary)
        {
            var button = new Button
            {
                Content = label,
                MinHeight = 34,
                Margin = new Thickness(0, 2, 0, 0),
                Padding = new Thickness(8, 5, 8, 5),
                FontWeight = FontWeights.SemiBold,
                Foreground = primary ? Brushes.White : TextBrush,
                Background = primary ? PrimaryBrush : SurfaceBrush,
                BorderBrush = primary ? PrimaryBrush : BorderColorBrush,
                HorizontalContentAlignment = HorizontalAlignment.Center
            };
            AutomationProperties.SetName(button, label);
            return button;
        }

        private async Task ProcessSelectionAsync(string operation)
        {
            double outline = 0;
            double diameter = 0;
            double clearance = 0;
            double tool = 0;
            double smoothingValue = 0;
            if (operation != "merge-holes"
                && (!TryReadPositive(outlineOffset, out outline)
                    || !TryReadNonNegative(holeDiameter, out diameter)
                    || !TryReadPositive(holeEdgeClearance, out clearance)
                    || !TryReadNonNegative(toolDiameter, out tool)
                    || !TryReadNonNegative(smoothing, out smoothingValue)))
            {
                SetStatus("巡边外扩、孔边距必须大于 0；孔径、道具口径和平滑值可设为 0。", ErrorBrush);
                return;
            }

            var executable = Path.Combine(
                Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location),
                "cdr-corel.exe");
            if (!File.Exists(executable))
            {
                SetStatus("缺少 cdr-corel.exe，请重新安装插件。", ErrorBrush);
                return;
            }

            SetRunning(true);
            progressBar.Value = 0;
            SetStatus("正在读取 CorelDRAW 当前选择…", TextBrush);
            try
            {
                var result = await RunProcessorAsync(
                    executable,
                    operation,
                    outline,
                    diameter,
                    clearance,
                    tool,
                    smoothingValue,
                    delegate(double value, string stage)
                    {
                        Dispatcher.BeginInvoke(new Action(delegate
                        {
                            progressBar.Value = Math.Max(0, Math.Min(100, value * 100));
                            SetStatus(stage, TextBrush);
                        }));
                    });
                if (result.ExitCode == 0)
                {
                    progressBar.Value = 100;
                    SetStatus(string.IsNullOrWhiteSpace(result.StandardOutput)
                        ? "处理完成。文档未自动保存。"
                        : result.StandardOutput.Trim(), SuccessBrush);
                }
                else
                {
                    var message = string.IsNullOrWhiteSpace(result.StandardError)
                        ? result.StandardOutput
                        : result.StandardError;
                    SetStatus("处理失败：" + message.Trim(), ErrorBrush);
                }
            }
            catch (Exception error)
            {
                SetStatus("处理失败：" + error.Message, ErrorBrush);
            }
            finally
            {
                SetRunning(false);
            }
        }

        private static async Task<ProcessResult> RunProcessorAsync(
            string executable,
            string operation,
            double outline,
            double diameter,
            double clearance,
            double tool,
            double smoothingValue,
            Action<double, string> progress)
        {
            string arguments;
            if (operation == "outline")
                arguments = string.Format(CultureInfo.InvariantCulture, "outline-selection {0:R} {1:R} {2:R}", outline, tool, smoothingValue);
            else if (operation == "trim")
                arguments = string.Format(CultureInfo.InvariantCulture, "trim-transparent-selection {0:R} {1:R} {2:R}", outline, tool, smoothingValue);
            else if (operation == "add-holes")
                arguments = string.Format(CultureInfo.InvariantCulture, "add-selection-holes {0:R} {1:R}", diameter, clearance);
            else if (operation == "export-selection")
                arguments = "export-selection-svg";
            else if (operation == "export-page")
                arguments = "export-page-svg";
            else
                arguments = "merge-selected-holes";
            var startInfo = new ProcessStartInfo(executable, arguments)
            {
                UseShellExecute = false,
                CreateNoWindow = true,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                StandardOutputEncoding = Encoding.UTF8,
                StandardErrorEncoding = Encoding.UTF8,
                WorkingDirectory = Path.GetDirectoryName(executable)
            };

            using (var process = new Process { StartInfo = startInfo })
            {
                process.Start();
                var standardOutput = ReadProcessorOutputAsync(process, progress);
                var standardError = process.StandardError.ReadToEndAsync();
                await Task.Run(delegate { process.WaitForExit(); });
                await Task.WhenAll(standardOutput, standardError);
                return new ProcessResult(
                    process.ExitCode,
                    standardOutput.Result,
                    standardError.Result);
            }
        }

        private static async Task<string> ReadProcessorOutputAsync(
            Process process,
            Action<double, string> progress)
        {
            var output = new StringBuilder();
            string line;
            while ((line = await process.StandardOutput.ReadLineAsync()) != null)
            {
                const string prefix = "__CDR_PROGRESS__\t";
                if (line.StartsWith(prefix, StringComparison.Ordinal))
                {
                    var fields = line.Substring(prefix.Length).Split('\t');
                    double value;
                    if (fields.Length >= 2
                        && double.TryParse(fields[0], NumberStyles.Float, CultureInfo.InvariantCulture, out value))
                    {
                        progress(value, string.Join(" ", fields, 1, fields.Length - 1));
                    }
                    continue;
                }
                output.AppendLine(line);
            }
            return output.ToString();
        }

        private static bool TryReadPositive(TextBox input, out double value)
        {
            var text = input.Text.Trim();
            var parsed = double.TryParse(text, NumberStyles.Float, CultureInfo.CurrentCulture, out value)
                || double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out value);
            input.BorderBrush = parsed && value > 0 && !double.IsInfinity(value)
                ? BorderColorBrush
                : ErrorBrush;
            return parsed && value > 0 && !double.IsInfinity(value) && !double.IsNaN(value);
        }

        private static bool TryReadNonNegative(TextBox input, out double value)
        {
            var text = input.Text.Trim();
            var parsed = double.TryParse(text, NumberStyles.Float, CultureInfo.CurrentCulture, out value)
                || double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out value);
            input.BorderBrush = parsed && value >= 0 && !double.IsInfinity(value)
                ? BorderColorBrush
                : ErrorBrush;
            return parsed && value >= 0 && !double.IsInfinity(value) && !double.IsNaN(value);
        }

        private void SetRunning(bool running)
        {
            outlineOffset.IsEnabled = !running;
            holeDiameter.IsEnabled = !running;
            holeEdgeClearance.IsEnabled = !running;
            toolDiameter.IsEnabled = !running;
            smoothing.IsEnabled = !running;
            outlineButton.IsEnabled = !running;
            trimButton.IsEnabled = !running;
            addHoleButton.IsEnabled = !running;
            mergeHolesButton.IsEnabled = !running;
            exportSelectionButton.IsEnabled = !running;
            exportPageButton.IsEnabled = !running;
            autoUploadPrintFlow.IsEnabled = !running;
        }

        private async Task ExportSvgAsync(string operation)
        {
            var executable = Path.Combine(
                Path.GetDirectoryName(Assembly.GetExecutingAssembly().Location),
                "cdr-corel.exe");
            if (!File.Exists(executable))
            {
                SetStatus("缺少 cdr-corel.exe，请重新安装插件。", ErrorBrush);
                return;
            }

            SetRunning(true);
            progressBar.Value = 0;
            SetStatus("正在导出 SVG…", TextBrush);
            try
            {
                var result = await RunProcessorAsync(
                    executable,
                    operation,
                    0,
                    0,
                    0,
                    0,
                    0,
                    delegate(double value, string stage)
                    {
                        Dispatcher.BeginInvoke(new Action(delegate
                        {
                            progressBar.Value = Math.Max(0, Math.Min(100, value * 100));
                            SetStatus(stage, TextBrush);
                        }));
                    });
                if (result.ExitCode != 0)
                {
                    var message = string.IsNullOrWhiteSpace(result.StandardError)
                        ? result.StandardOutput
                        : result.StandardError;
                    SetStatus("导出失败：" + message.Trim(), ErrorBrush);
                    return;
                }

                var output = ExtractExportPath(result.StandardOutput);
                if (autoUploadPrintFlow.IsChecked == true)
                {
                    if (output == null)
                    {
                        SetStatus("导出成功但未找到输出 SVG 路径。", ErrorBrush);
                        return;
                    }
                    var uploadMessage = await Task.Run(delegate { return UploadToPrintFlow(output); });
                    var focused = Dispatcher.CheckAccess()
                        ? FocusPrintFlowWindow()
                        : (bool)Dispatcher.Invoke(new Func<bool>(FocusPrintFlowWindow));
                    var focusMessage = focused
                        ? "SVG 已提交 PrintFlow，窗口已切到前台。"
                        : "SVG 已提交 PrintFlow，但未能自动切换窗口；请手动切换到 PrintFlow。";
                    SetStatus(focusMessage, SuccessBrush);
                    statusText.ToolTip = result.StandardOutput.Trim() + "\n" + uploadMessage + "\n" + focusMessage;
                }
                else
                {
                    SetStatus(result.StandardOutput.Trim(), SuccessBrush);
                }
                progressBar.Value = 100;
            }
            catch (Exception error)
            {
                SetStatus("导出失败：" + error.Message, ErrorBrush);
            }
            finally
            {
                SetRunning(false);
            }
        }

        private static string ExtractExportPath(string output)
        {
            foreach (var line in output.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries))
            {
                if (line.StartsWith("__CDR_EXPORT__\t", StringComparison.Ordinal))
                    return line.Substring("__CDR_EXPORT__\t".Length).Trim();
            }
            return null;
        }

        private static string UploadToPrintFlow(string path)
        {
            if (!Path.IsPathRooted(path) || !File.Exists(path))
                throw new InvalidOperationException("SVG 文件不存在：" + path);
            var endpoint = DiscoverPrintFlowEndpoint();
            var jsonPath = path.Replace("\\", "\\\\").Replace("\"", "\\\"");
            using (var client = new WebClient { Encoding = Encoding.UTF8 })
            {
                client.Headers[HttpRequestHeader.ContentType] = "application/json";
                var response = client.UploadString(
                    endpoint + "/api/import-svg",
                    "POST",
                    "{\"path\":\"" + jsonPath + "\"}");
                return "已提交 PrintFlow：" + path;
            }
        }

        private static string DiscoverPrintFlowEndpoint()
        {
            var cache = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "printflow-data", "cache", "local-api.json");
            var candidates = new System.Collections.Generic.List<string>();
            if (File.Exists(cache))
            {
                var json = File.ReadAllText(cache);
                var url = Regex.Match(json, "\\\"url\\\"\\s*:\\s*\\\"(http://127\\.0\\.0\\.1:[0-9]+)\\\"");
                if (url.Success) candidates.Add(url.Groups[1].Value);
            }
            for (var port = 47821; port <= 47840; port++)
                candidates.Add("http://127.0.0.1:" + port.ToString(CultureInfo.InvariantCulture));
            using (var client = new WebClient())
            {
                foreach (var candidate in candidates)
                {
                    try
                    {
                        client.DownloadString(candidate + "/api/health");
                        return candidate;
                    }
                    catch (WebException) { }
                }
            }
            throw new InvalidOperationException("未找到 PrintFlow 本地 API；请先启动 PrintFlow");
        }

        private static bool FocusPrintFlowWindow()
        {
            var window = FindWindow(null, "PrintFlow");
            if (window == IntPtr.Zero)
                return false;

            ShowWindow(window, SwRestore);
            return SetForegroundWindow(window);
        }

        private void SetStatus(string message, Brush brush)
        {
            statusText.Text = message;
            statusText.ToolTip = message;
            statusText.Foreground = brush;
        }

        private static Brush BrushFrom(string value)
        {
            var brush = (SolidColorBrush)new BrushConverter().ConvertFromString(value);
            brush.Freeze();
            return brush;
        }

        private sealed class ProcessResult
        {
            public ProcessResult(int exitCode, string standardOutput, string standardError)
            {
                ExitCode = exitCode;
                StandardOutput = standardOutput;
                StandardError = standardError;
            }

            public int ExitCode { get; private set; }
            public string StandardOutput { get; private set; }
            public string StandardError { get; private set; }
        }
    }
}
