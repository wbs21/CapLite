# 启动耗时：改后（release，FFmpeg DLL 延迟加载 + 枚举完成后预加载）与
#         改前基线（release，CAP_NO_DELAYLOAD=1 链接，DLL 隐式映射）的实测对比。
#
# 用法（在 CapLite 根目录）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\startup-compare.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\startup-compare.ps1 -Runs 3 -Label release-after
#
# 测什么（全部来自真实进程，不伪造、不估算）：
#   1. `[cap][t=..ms] phase=..` 打点时间轴（cap.log，t=0 = main() 第一行）；
#   2. 进程创建 -> 主窗口可见的墙钟（外部 5ms 轮询 MainWindowHandle，5ms 是它的分辨率极限）；
#   3. 进程创建 -> 首个 av*/sw* 模块被加载（无预加载的基线里这就是"隐式映射完成"的时刻）；
#   4. 打点归零：`process_start` 的 t 值 = 动态链接/CRT 启动那段（进程内拿不到 Instant）。
#      注意：这个阶段点在某些构建里**不出现**（本文件用 0 兜底，并在 JSON 里如实标注）。
#
# 四个口径：
#   1 进程创建 -> 首帧        = t_z + first_frame.t
#   2 首帧 -> 枚举完成        = deferred_enum_done.t - first_frame.t
#   3 枚举完成 -> 预加载完成  = ffmpeg_preload_done.t - deferred_enum_done.t
#   4 进程创建 -> 预加载完成  = t_z + ffmpeg_preload_done.t
#
# ---------------------------------------------------------------------------
# 实现注意（Windows PowerShell 5.1 的坑，都实际踩过，别改回去）
# ---------------------------------------------------------------------------
#   * 参数叫 $Runs，所以**不要**再用 $runs 累积结果 —— PS 变量名大小写不敏感，
#     `$runs = ...` 就是在给 [int]$Runs 赋值，报错却是
#     `Cannot convert the "…List`1[Object]" value … to type "System.Int32"`，
#     位置还指到别处（本文件用 $samples）；
#   * `$arr += [ordered]@{...}` 会去找 OrderedDictionary.op_Addition 并失败；
#   * 函数返回字典会被管道枚举成条目数组 —— 本文件一律用"传入字典就地填充"；
#   * `dur` 可能是整数（`dur=0ms`），正则的小数部分必须可选；
#   * 本文件必须是 **UTF-8 with BOM**：PS 5.1 对含中文的无 BOM 脚本按 ANSI 解析 → ParserError。

[CmdletBinding()]
param(
    # 运行次数（第 1 次为 first，其余为 warm）
    [int]$Runs = 3,
    # 结果标签（写进 JSON 文件名与表头）
    [string]$Label = 'release-after',
    # 被测 exe（默认 <CARGO_TARGET_DIR 或 工作区\_rust\target-cap>\release\CapLite.exe）
    [string]$Exe = '',
    # 独立配置目录（避免污染用户配置；cap.log 落在这里）
    [string]$WorkDir = '',
    # 抓 av*/sw* 模块的上限时间（秒）
    [int]$TimeoutSec = 60,
    # 结果 JSON 输出目录
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
$Workspace = Split-Path -Parent $Root
$TargetDir = Join-Path $Workspace '_rust\target-cap'
if ($env:CARGO_TARGET_DIR) { $TargetDir = $env:CARGO_TARGET_DIR }

if ([string]::IsNullOrWhiteSpace($Exe)) { $Exe = Join-Path $TargetDir 'release\CapLite.exe' }
if (-not (Test-Path -LiteralPath $Exe)) { throw "找不到 release 产物：$Exe（先 cargo build --release --bin CapLite）" }
$Exe = (Resolve-Path -LiteralPath $Exe).Path

if ([string]::IsNullOrWhiteSpace($WorkDir)) { $WorkDir = Join-Path $TargetDir ("startup-compare-" + $Label) }
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null
$WorkDir = (Resolve-Path -LiteralPath $WorkDir).Path
$LogFile = Join-Path $WorkDir 'cap.log'

if ([string]::IsNullOrWhiteSpace($OutDir)) { $OutDir = Join-Path $TargetDir 'startup-bench' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$script:PhaseRe = [regex]'\[cap\]\[t=(\d+)ms\] phase=(\S+) scope=(\S+) dur=(\d+(?:\.\d+)?)ms(?: extra="([^"]*)")?'

function Read-LogText {
    if (-not (Test-Path -LiteralPath $LogFile)) { return '' }
    try {
        $t = [System.IO.File]::ReadAllText($LogFile, [System.Text.Encoding]::UTF8)
        return $t.TrimStart([char]0xFEFF)
    } catch { return '' }
}

# 就地填充传入的字典（不靠函数返回值）；返回匹配到的阶段条数
function Fill-Phases {
    param([hashtable]$Into)
    $text = Read-LogText
    if ($text.Length -eq 0) { return 0 }
    $n = 0
    foreach ($m in $script:PhaseRe.Matches($text)) {
        $Into[$m.Groups[2].Value] = @{
            t     = [int]$m.Groups[1].Value
            dur   = [double]$m.Groups[4].Value
            extra = $m.Groups[5].Value
        }
        $n++
    }
    return $n
}

function Wait-Phase {
    param([string]$Phase, [int]$TimeoutSec = 20)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ((Read-LogText).Contains("phase=$Phase")) { return $true }
        Start-Sleep -Milliseconds 10
    }
    return $false
}

function Get-FfmpegModules {
    param([int]$ProcessId)
    try {
        $mods = @((Get-Process -Id $ProcessId -ErrorAction Stop).Modules |
            Where-Object { $_.ModuleName -like 'av*' -or $_.ModuleName -like 'sw*' } |
            Select-Object -ExpandProperty ModuleName)
        return $mods
    } catch { return @() }
}

# 就地填充统计字典
function Fill-Stat {
    param([hashtable]$Into, [string]$Key, $Values)
    $vals = @($Values | Where-Object { $_ -ne $null } | Sort-Object)
    if ($vals.Count -eq 0) { return }
    $n = $vals.Count
    $mid = [int][Math]::Floor($n / 2)
    if ($n % 2 -eq 1) { $med = $vals[$mid] } else { $med = ($vals[$mid - 1] + $vals[$mid]) / 2 }
    $Into[$Key] = @{
        n      = $n
        min    = [Math]::Round([double]$vals[0], 1)
        median = [Math]::Round([double]$med, 1)
        max    = [Math]::Round([double]$vals[$n - 1], 1)
    }
}

# 跑一次；结果写进传入的字典（避免函数返回字典被枚举）
function Invoke-CapRun {
    param([string]$Tag, [hashtable]$Into)

    if (Test-Path -LiteralPath $LogFile) { Remove-Item -LiteralPath $LogFile -Force }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.Arguments = '--config-dir "' + $WorkDir + '"'
    $psi.UseShellExecute = $false
    $proc = [System.Diagnostics.Process]::Start($psi)

    $windowMs = $null
    $firstModuleMs = $null
    $moduleNames = @{}
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($windowMs -eq $null) {
            try {
                $proc.Refresh()
                if ($proc.MainWindowHandle -ne 0) { $windowMs = $sw.Elapsed.TotalMilliseconds }
            } catch { }
        }
        foreach ($m in (Get-FfmpegModules -ProcessId $proc.Id)) {
            if (-not $moduleNames.ContainsKey($m)) {
                $moduleNames[$m] = $true
                if ($firstModuleMs -eq $null) { $firstModuleMs = $sw.Elapsed.TotalMilliseconds }
            }
        }
        if (($windowMs -ne $null) -and ($firstModuleMs -ne $null)) { break }
        if ($proc.HasExited) { break }
        Start-Sleep -Milliseconds 5
    }

    Wait-Phase -Phase 'ffmpeg_preload_done' -TimeoutSec 20 | Out-Null
    Start-Sleep -Milliseconds 150

    $phases = @{}
    $phaseCount = Fill-Phases -Into $phases
    $peakBytes = $null
    try { $proc.Refresh(); $peakBytes = $proc.PeakWorkingSet64 } catch { }

    if (-not $proc.HasExited) { $proc.Kill() | Out-Null }
    Start-Sleep -Milliseconds 350

    $tz = 0
    $hasTz = $false
    if ($phases.ContainsKey('process_start')) { $tz = $phases['process_start'].t; $hasTz = $true }
    $first = $null; $enum = $null; $pre = $null
    if ($phases.ContainsKey('ui_first_frame_done')) { $first = $phases['ui_first_frame_done'].t }
    if ($phases.ContainsKey('deferred_enum_done')) { $enum = $phases['deferred_enum_done'].t }
    if ($phases.ContainsKey('ffmpeg_preload_done')) { $pre = $phases['ffmpeg_preload_done'].t }

    $derr = $null; $dpre = $null; $wfirst = $null; $wpre = $null
    $pdur = $null; $pex = $null; $enumAbs = $null
    if (($enum -ne $null) -and ($first -ne $null)) { $derr = $enum - $first }
    if (($pre -ne $null) -and ($enum -ne $null)) { $dpre = $pre - $enum }
    if ($first -ne $null) { $wfirst = $tz + $first }
    if ($pre -ne $null) { $wpre = $tz + $pre }
    if ($enum -ne $null) { $enumAbs = $tz + $enum }
    if ($phases.ContainsKey('ffmpeg_preload_done')) {
        $pdur = $phases['ffmpeg_preload_done'].dur
        $pex = $phases['ffmpeg_preload_done'].extra
    }

    $Into['tag'] = $Tag
    $Into['window_visible_ms'] = if ($windowMs -ne $null) { [Math]::Round([double]$windowMs, 1) } else { $null }
    $Into['first_ffmpeg_module_ms'] = if ($firstModuleMs -ne $null) { [Math]::Round([double]$firstModuleMs, 1) } else { $null }
    $Into['ffmpeg_modules_seen'] = @($moduleNames.Keys | Sort-Object)
    $Into['phase_count'] = $phaseCount
    $Into['has_process_start_mark'] = $hasTz
    $Into['t_zero_ms'] = $tz
    $Into['phase_t'] = $phases
    $Into['wall_first_frame_ms'] = $wfirst
    $Into['first_to_enum_ms'] = $derr
    $Into['enum_to_preload_ms'] = $dpre
    $Into['wall_preload_done_ms'] = $wpre
    $Into['enum_abs_ms'] = $enumAbs
    $Into['preload_dur_ms'] = $pdur
    $Into['peak_working_set_bytes'] = $peakBytes
    $Into['extra_preload'] = $pex
    return $true
}

# ---------------------------------------------------------------------------
# 运行
# ---------------------------------------------------------------------------
$exeItem = Get-Item -LiteralPath $Exe
$dllA = Join-Path (Split-Path -Parent $Exe) 'avcodec-63.dll'
Write-Host ('=== 启动耗时对比（{0}）===' -f $Label) -ForegroundColor Cyan
Write-Host ('  exe    : {0}' -f $Exe)
Write-Host ('  exe 大小/时间 : {0:N0} B / {1}' -f $exeItem.Length, $exeItem.LastWriteTime.ToString('s'))
if (Test-Path -LiteralPath $dllA) {
    $dllItem = Get-Item -LiteralPath $dllA
    if ($dllItem.Length -gt 10000000) { $dllKind = '   <- BtbN 全功能版' } else { $dllKind = '   <- 自建最小版' }
    Write-Host ('  avcodec-63.dll: {0:N0} B / {1}{2}' -f $dllItem.Length, $dllItem.LastWriteTime.ToString('s'), $dllKind)
}

$samples = @()
for ($i = 0; $i -lt $Runs; $i++) {
    if ($i -eq 0) { $tag = 'first' } else { $tag = 'warm' + $i }
    $r = @{}
    Invoke-CapRun -Tag $tag -Into $r | Out-Null
    $samples = $samples + @($r)
    $line = '  [{0,-5}] 窗口 {1} ms | 首个 av*/sw* 模块 {2} ms | t_z={3} ms | 首帧@+{4} ms | 枚举@+{5} ms | 预加载@+{6} ms (dur {7} ms, 阶段 {8} 个)' -f `
        $tag, $r['window_visible_ms'], $r['first_ffmpeg_module_ms'], $r['t_zero_ms'],
        $r['wall_first_frame_ms'], $r['enum_abs_ms'], $r['wall_preload_done_ms'],
        $r['preload_dur_ms'], $r['phase_count']
    Write-Host $line
}

$warm = @($samples | Where-Object { $_.tag -ne 'first' })
if ($warm.Count -eq 0) { $warm = $samples }

$summary = @{}
Fill-Stat -Into $summary -Key 'wall_first_frame_ms'    -Values @($warm | ForEach-Object { $_.wall_first_frame_ms })
Fill-Stat -Into $summary -Key 'first_to_enum_ms'       -Values @($warm | ForEach-Object { $_.first_to_enum_ms })
Fill-Stat -Into $summary -Key 'enum_to_preload_ms'     -Values @($warm | ForEach-Object { $_.enum_to_preload_ms })
Fill-Stat -Into $summary -Key 'wall_preload_done_ms'   -Values @($warm | ForEach-Object { $_.wall_preload_done_ms })
Fill-Stat -Into $summary -Key 'preload_dur_ms'         -Values @($warm | ForEach-Object { $_.preload_dur_ms })
Fill-Stat -Into $summary -Key 'window_visible_ms'      -Values @($warm | ForEach-Object { $_.window_visible_ms })
Fill-Stat -Into $summary -Key 'first_ffmpeg_module_ms' -Values @($warm | ForEach-Object { $_.first_ffmpeg_module_ms })
Fill-Stat -Into $summary -Key 'peak_working_set_mb'    -Values @($warm | ForEach-Object { if ($_.peak_working_set_bytes) { [Math]::Round($_.peak_working_set_bytes / 1MB, 1) } else { $null } })

$result = @{
    label                  = $Label
    exe                    = $Exe
    exe_bytes              = $exeItem.Length
    exe_written            = $exeItem.LastWriteTime.ToString('s')
    runs                   = $samples
    warm_summary           = $summary
    has_process_start_mark = $samples[0]['has_process_start_mark']
    note                   = '1=进程创建->首帧；2=首帧->枚举完成；3=枚举完成->预加载完成；4=进程创建->预加载完成；t_z=process_start 打点（缺失时按 0 计）'
}
$jsonPath = Join-Path $OutDir ($Label + '.json')
$result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $jsonPath -Encoding UTF8

Write-Host ''
Write-Host ('=== 汇总（热启动 {0} 次的 min / 中位 / max，单位 ms）===' -f $warm.Count) -ForegroundColor Cyan
Write-Host ('{0,-34} {1,9} {2,9} {3,9}' -f '口径', 'min', 'median', 'max')
$rows = @(
    @('1 进程创建 -> 首帧', 'wall_first_frame_ms'),
    @('2 首帧 -> 枚举完成', 'first_to_enum_ms'),
    @('3 枚举完成 -> 预加载完成', 'enum_to_preload_ms'),
    @('4 进程创建 -> 预加载完成', 'wall_preload_done_ms'),
    @('  预加载自身耗时', 'preload_dur_ms'),
    @('  主窗口可见（外部墙钟）', 'window_visible_ms'),
    @('  首个 av*/sw* 模块加载', 'first_ffmpeg_module_ms'),
    @('  峰值工作集 (MB)', 'peak_working_set_mb')
)
foreach ($row in $rows) {
    if (-not $summary.ContainsKey($row[1])) {
        Write-Host ('{0,-34} {1,>9} {2,>9} {3,>9}' -f $row[0], '-', '-', '-')
    } else {
        $v = $summary[$row[1]]
        Write-Host ('{0,-34} {1,9} {2,9} {3,9}' -f $row[0], $v.min, $v.median, $v.max)
    }
}
Write-Host ''
Write-Host ('JSON: ' + $jsonPath) -ForegroundColor Green
