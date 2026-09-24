param(
    [string]$PackageRoot = (Join-Path $PSScriptRoot '..\crates\cdr-plugin\package\content'),
    [string]$PluginSource = (Join-Path $PSScriptRoot '..\crates\cdr-plugin\Plugin.cs')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$appUiPath = Join-Path $PackageRoot 'AppUI.xslt'
$userUiPath = Join-Path $PackageRoot 'UserUI.xslt'
if (-not (Test-Path -LiteralPath $appUiPath -PathType Leaf)) {
    throw "Missing Corel application UI modification: $appUiPath"
}
if (-not (Test-Path -LiteralPath $userUiPath -PathType Leaf)) {
    throw "Missing Corel workspace UI modification: $userUiPath"
}

[xml]$appUi = Get-Content -LiteralPath $appUiPath -Raw
$namespace = [Xml.XmlNamespaceManager]::new($appUi.NameTable)
$namespace.AddNamespace('xsl', 'http://www.w3.org/1999/XSL/Transform')

$wpfHost = $appUi.SelectSingleNode(
    "//xsl:template[@match='uiConfig/items']//itemData[@type='wpfhost']",
    $namespace)
if ($null -eq $wpfHost) {
    throw 'AppUI.xslt does not declare a WPF host.'
}

$expectedBinding = '*Bind(DataSource=CdrOutlineDatasource;Path=DialogContent)'
if ($wpfHost.hostedType -cne $expectedBinding) {
    throw "Unexpected WPF hostedType: $($wpfHost.hostedType)"
}

$docker = $appUi.SelectSingleNode(
    "//xsl:template[@match='uiConfig/dockers']//dockerData",
    $namespace)
if ($null -eq $docker) {
    throw 'AppUI.xslt does not declare a Corel docker.'
}

$hostItem = $docker.SelectSingleNode('.//item[@dock="fill"]')
if ($null -eq $hostItem -or $hostItem.guidRef -cne $wpfHost.guid) {
    throw 'The docker does not fill its content with the declared WPF host.'
}

$source = Get-Content -LiteralPath $PluginSource -Raw
if ($source -notmatch [regex]::Escape('FrameWork.ShowDocker(DockerGuid)')) {
    throw 'The plugin menu handler does not show the Corel docker.'
}
if ($source -match [regex]::Escape('FrameWork.ShowDialog(')) {
    throw 'The plugin still opens a dialog instead of its docker.'
}
if ($source -notmatch [regex]::Escape('CreateParameterRow("线条平滑", smoothing)')) {
    throw 'The plugin panel does not expose the line smoothing parameter.'
}
if ($source -notmatch [regex]::Escape('CreateParameterRow("道具口径", toolDiameter)')) {
    throw 'The plugin panel does not expose the tool diameter parameter.'
}
foreach ($command in @('outline-selection', 'add-selection-holes', 'trim-transparent-selection')) {
    if ($source -notmatch [regex]::Escape('"' + $command + ' ')) {
        throw "The plugin does not expose the $command Rust operation."
    }
}
if ($source -notmatch [regex]::Escape('"merge-selected-holes"')) {
    throw 'The plugin does not expose the merge-selected-holes Rust operation.'
}
if ($source -notmatch [regex]::Escape('ReadProcessorOutputAsync')) {
    throw 'The plugin does not read progress updates from the Rust processor.'
}

$layoutMatch = [regex]::Match(
    $source,
    '(?s)private UIElement BuildLayout\(\)(.*?)private static Border CreateSection')
if (-not $layoutMatch.Success) {
    throw 'Could not locate the Corel panel layout for visibility checks.'
}
$layout = $layoutMatch.Groups[1].Value
if ($layout -cmatch 'content\.Children\.Add\((progressBar|statusText)\)') {
    throw 'The progress bar and status message must stay outside the scrolling controls.'
}
if ($layout -notmatch 'var statusFooter\s*=\s*new Border') {
    throw 'The Corel panel does not define a persistent status footer.'
}
if ($layout -notmatch 'WpfGrid\.SetRow\(statusFooter,\s*2\)' -or $layout -notmatch 'root\.Children\.Add\(statusFooter\)') {
    throw 'The persistent status footer is not placed in its own root-grid row.'
}
$trimButtonPosition = $layout.IndexOf('content.Children.Add(trimButton);', [StringComparison]::Ordinal)
$outlineSectionPosition = $layout.IndexOf('content.Children.Add(CreateSection("巡边", outlineSection));', [StringComparison]::Ordinal)
if ($trimButtonPosition -lt 0 -or $outlineSectionPosition -lt 0 -or $trimButtonPosition -ge $outlineSectionPosition) {
    throw 'The transparent-edge button must be the first control in the panel.'
}

Write-Output 'COREL_PLUGIN_PACKAGE=PASS'
