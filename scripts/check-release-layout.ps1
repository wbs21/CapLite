# Enforce the canonical CapLite release layout.
# Usage: pwsh -NoProfile -File scripts\check-release-layout.ps1 [-Root <folder>]
param(
    [string]$Root = ''
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$project = Split-Path -Parent $scriptDir
if ([string]::IsNullOrWhiteSpace($Root)) {
    $Root = Join-Path $project 'dist\CapLite-1.0.1'
}
$Root = (Resolve-Path -LiteralPath $Root).Path

$rootFiles = @('CapLite.exe', 'README.md')
$runtimeDlls = @(
    'avcodec-63.dll',
    'avformat-63.dll',
    'avutil-61.dll',
    'swresample-7.dll',
    'swscale-10.dll',
    'openh264.dll',
    'openh264-7.dll'
)
$rootDirs = @('runtime', 'docs', 'licenses')
$docs = @('README.txt', 'screenshots\record.jpg', 'screenshots\trim.jpg', 'screenshots\settings.jpg')
$licenses = @('LICENSE', 'LICENSE-FFmpeg.txt', 'LICENSE-OpenH264.txt', 'THIRD-PARTY-NOTICES.txt')

$errors = New-Object System.Collections.Generic.List[string]

$actualRootFiles = @(Get-ChildItem -LiteralPath $Root -File -Force | Select-Object -ExpandProperty Name)
foreach ($f in $rootFiles) {
    if ($actualRootFiles -notcontains $f) { $errors.Add("missing root file: $f") }
}
foreach ($f in $actualRootFiles) {
    if ($rootFiles -notcontains $f) { $errors.Add("unexpected root file: $f") }
}

$actualRootDirs = @(Get-ChildItem -LiteralPath $Root -Directory -Force | Select-Object -ExpandProperty Name)
foreach ($d in $rootDirs) {
    if ($actualRootDirs -notcontains $d) { $errors.Add("missing folder: $d") }
}
foreach ($d in $actualRootDirs) {
    if ($rootDirs -notcontains $d) { $errors.Add("unexpected root folder: $d") }
}

$actualRuntime = @(Get-ChildItem -LiteralPath (Join-Path $Root 'runtime') -File -Force | Select-Object -ExpandProperty Name)
foreach ($f in $runtimeDlls) {
    if ($actualRuntime -notcontains $f) { $errors.Add("missing runtime\$f") }
}
foreach ($f in $actualRuntime) {
    if ($runtimeDlls -notcontains $f) { $errors.Add("unexpected runtime file: $f") }
}

foreach ($f in $docs) {
    if (-not (Test-Path -LiteralPath (Join-Path $Root "docs\$f"))) { $errors.Add("missing docs\$f") }
}
foreach ($f in $licenses) {
    if (-not (Test-Path -LiteralPath (Join-Path $Root "licenses\$f"))) { $errors.Add("missing licenses\$f") }
}

if ($errors.Count -gt 0) {
    Write-Host 'FAIL: release layout does not match docs\RELEASE_LAYOUT.md' -ForegroundColor Red
    $errors | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "PASS: $Root" -ForegroundColor Green
Write-Host "  root: $($rootFiles.Count) files + 3 folders | runtime: $($runtimeDlls.Count) DLLs | docs: $($docs.Count) | licenses: $($licenses.Count)"
