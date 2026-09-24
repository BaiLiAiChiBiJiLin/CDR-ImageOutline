param(
    [string]$ExpectedGuid = '5da381bf-9571-4435-a8eb-27ad9a0ef750'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Get-Process -Name CorelDRW -ErrorAction SilentlyContinue)) {
    throw 'CorelDRAW 2020 must be running before checking the plugin menu.'
}

$application = New-Object -ComObject 'CorelDRAW.Application.22'
$framework = $application.FrameWork
$automation = $framework.Automation
$matches = [Collections.Generic.List[object]]::new()
$visited = @{}

function Visit-MenuBar {
    param(
        [Parameter(Mandatory = $true)][string]$BarId,
        [Parameter(Mandatory = $true)][string]$MenuPath
    )

    if ($visited.ContainsKey($BarId)) { return }
    $visited[$BarId] = $true

    $count = $automation.GetNumItemsOnBar($BarId)
    for ($index = 0; $index -lt $count; $index++) {
        $separator = $false
        $controlId = $automation.GetItem($BarId, $index, [ref]$separator)
        if ($separator -or [string]::IsNullOrWhiteSpace($controlId)) { continue }

        $caption = $automation.GetCaptionText($controlId)
        $itemPath = if ([string]::IsNullOrWhiteSpace($caption)) {
            $MenuPath
        } else {
            "$MenuPath > $caption"
        }

        if ($controlId -ieq $ExpectedGuid) {
            $matches.Add([PSCustomObject]@{
                Id = $controlId
                Caption = $caption
                Path = $itemPath
            })
        }

        $subBar = ''
        if ($automation.GetSubBar($controlId, [ref]$subBar)) {
            Visit-MenuBar -BarId $subBar -MenuPath $itemPath
        }
    }
}

$mainMenu = $framework.MainMenu
for ($index = 1; $index -le $mainMenu.Controls.Count; $index++) {
    $control = $mainMenu.Controls.Item($index)
    $subBar = ''
    if ($automation.GetSubBar($control.ID, [ref]$subBar)) {
        Visit-MenuBar -BarId $subBar -MenuPath $control.Caption
    }
}

if ($matches.Count -eq 0) {
    Write-Error "Corel menu entry is missing: $ExpectedGuid"
    exit 1
}

$matches | Format-Table Id, Caption, Path -AutoSize
Write-Output 'COREL_PLUGIN_MENU=PASS'
