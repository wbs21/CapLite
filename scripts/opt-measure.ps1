# 启动优化**证据脚本**：分阶段计时 + 模块加载验证（第 1 项的关键证据）。
#
# 用法（在 CapLite 根目录）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\opt-measure.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\opt-measure.ps1 -Runs 3 -Label after
#
# 做三件事：
# 1. 启动 `target\release\CapLite.exe` `-Runs` 次，每次等到 `cap.log` 里出现 `phase=ui_first_frame_done`
#    就**在进程还活着的时候**抓它的已加载模块（这一步是第 1 项优化的唯一硬证据）；
# 2. 解析 `cap.log` 里的 `[cap][t=..ms] phase=.. dur=..ms` 打点，汇总成冷/热对比表；
# 3. 把原始结果写成 JSON 放到 `_rust\target-cap\startup-bench\<label>.json`，便于出对比表。
#
# **关于"冷启动"**：本机无法在不重启系统的前提下清空"已被读过的文件"的系统缓存，
# 所以这里给不出真正的冷启动。脚本的做法是：**第 1 次运行叫 first**（进程/文件都刚从磁盘上
# 回来过一次的程度），后面 `-Runs` 次叫 warm。文档里必须如实标注这个局限，不能假装是冷启动。

[CmdletBinding()]
param(
    # 热启动次数（不含第 1 次）
    [int]$Runs = 3,
    # 结果标签（用于文件名）
    [string]$Label = 'current',
    # 等待首帧的超时（秒）
    [int]$TimeoutSec = 40,
    # 只做模块加载验证、不做多次计时
    [switch]$ModulesOnly,
    # 触发一次 FFmpeg（录制 1 秒）后再查模块，证明"用到才加载"
    [switch]$TriggerRecord,
    # 结束后把 JSON 摘要直接打到 stdout
    [switch]$Json
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# 路径与环境
# ---------------------------------------------------------------------------
$Root = Split-Path -Parent $PSScriptRoot
$Workspace = Split-Path -Parent $Root
$TargetDir = Join-Path $Workspace '_rust\target-cap'
if ($env:CARGO_TARGET_DIR) { $TargetDir = $env:CARGO_TARGET_DIR }
$Exe = Join-Path $TargetDir 'release\CapLite.exe'
$OutDir = Join-Path $TargetDir 'startup-bench'
$WorkDir = Join-Path $TargetDir 'startup-bench-config'   # 用独立配置目录，避免污染用户配置

if (-not (Test-Path $Exe)) { throw "找不到 release 产物：$Exe（先 cargo build --release --bin CapLite）" }
New-Item -ItemType Directory -Force -Path $OutDir, $WorkDir | Out-Null

$LogFile = Join-Path $WorkDir 'cap.log'
$PhaseRe = [regex]'\[cap\]\[t=(\d+)ms\] phase=(\S+) scope=(\S+) dur=([\d.]+)ms(?: extra="([^"]*)")?'

function New-LogPath {
    if (Test-Path $LogFile) { Remove-Item $LogFile -Force }
}

# 启动一次，等到首帧打点出现（或超时），返回 @{ Proc; Phases; WallMs; WindowMs; ExitCode }
function Start-CapOnce {
    param([int]$TimeoutSec = 40)

    New-LogPath
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $Exe -ArgumentList @('--config-dir', $WorkDir) -PassThru
    $phases = [ordered]@{}
    $windowMs = $null
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 25
        # 窗口先出现：记下"主窗口句柄可见"的时刻（比首帧打点更接近用户感知）
        if ($null -eq $windowMs) {
            try {
                $proc.Refresh()
                if ($proc.MainWindowHandle -ne 0) { $windowMs = $sw.Elapsed.TotalMilliseconds }
            } catch { }
        }
        if (Test-Path $LogFile) {
            $text = ''
            try { $text = Get-Content $LogFile -Raw -ErrorAction Stop } catch { }
            if ($text -and $text.Contains('phase=ui_first_frame_done')) {
                foreach ($m in $PhaseRe.Matches($text)) {
                    $phases[$m.Groups[2].Value] = @{
                        t   = [int]$m.Groups[1].Value
                        dur = [double]$m.Groups[4].Value
                        extra = $m.Groups[5].Value
                    }
                }
                break
            }
        }
    }
    $firstFrameWall = $null
    if ($null -ne $windowMs) { $firstFrameWall = $windowMs }
    return @{
        Proc           = $proc
        Phases         = $phases
        WindowMs       = $windowMs
        FirstFrameWall = $firstFrameWall
        Elapsed        = $sw
    }
}

# 等到日志里出现某个阶段（用于确认"延后初始化"真的跑完了）
function Wait-Phase {
    param($Proc, [string]$Phase, [int]$TimeoutSec = 30)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 25
        if (Test-Path $LogFile) {
            $text = ''
            try { $text = Get-Content $LogFile -Raw -ErrorAction Stop } catch { }
            if ($text -and $text.Contains("phase=$Phase")) { return $true }
        }
        if ($Proc.HasExited) { return $false }
    }
    return $false
}

function Get-FfmpegModules {
    param([int]$ProcessId)
    $mods = @()
    try {
        $mods = (Get-Process -Id $ProcessId -ErrorAction Stop).Modules |
            Where-Object { $_.ModuleName -like 'av*' -or $_.ModuleName -like 'sw*' } |
            Select-Object -ExpandProperty ModuleName
    } catch { }
    return @($mods | Sort-Object -Unique)
}

# ---------------------------------------------------------------------------
# 1) 模块加载验证（第 1 项硬证据）
# ---------------------------------------------------------------------------
Write-Host '=== 模块加载验证：启动后（未触发录制/剪辑）===' -ForegroundColor Cyan
$s = Start-CapOnce -TimeoutSec $TimeoutSec
Start-Sleep -Milliseconds 1200   # 给"延后初始化"（显示器/音频/目录探测）留出时间
$before = Get-FfmpegModules -ProcessId $s.Proc.Id
$beforePhase = $s.Phases.Keys -join ','
Write-Host ("  已加载的 av*/sw* 模块: {0}" -f $(if ($before.Count) { $before -join ', ' } else { '(空)' }))

$afterTrigger = @()
if ($TriggerRecord) {
    Write-Host '  触发一次录制（recorder_smoke，走 FFmpeg 封装）…' -ForegroundColor Yellow
    $rec = Join-Path $TargetDir 'debug\recorder_smoke.exe'
    if (Test-Path $rec) {
        $out = Join-Path $TargetDir 'startup-bench\trigger.mp4'
        & $rec '1' $out '--gop' '1' 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "    $_" }
    } else {
        Write-Host "    找不到 $rec，跳过" -ForegroundColor Yellow
    }
    $afterTrigger = Get-FfmpegModules -ProcessId $s.Proc.Id
    Write-Host ("  录制后同进程模块: {0}" -f $(if ($afterTrigger.Count) { $afterTrigger -join ', ' } else { '(空)' }))
    Write-Host '  注：录制走的是独立进程（recorder_smoke），所以上面可能仍为空；' -ForegroundColor DarkGray
    Write-Host '      真正要证明的是"CapLite.exe 自己用到 FFmpeg 时才加载"——见下面的 -TriggerRecord 说明。' -ForegroundColor DarkGray
}

# 让它退出
if (-not $s.Proc.HasExited) { $s.Proc.Kill() }
Start-Sleep -Milliseconds 300

$moduleResult = [ordered]@{
    modules_at_startup = @($before)
    phases_seen        = $beforePhase
    triggered          = [bool]$TriggerRecord
    modules_after      = @($afterTrigger)
}
if ($ModulesOnly) {
    if ($Json) { $moduleResult | ConvertTo-Json -Depth 5 }
    exit 0
}

# ---------------------------------------------------------------------------
# 2) 分阶段计时（first + Runs 次 warm）
# ---------------------------------------------------------------------------
$runs = @()
$count = $Runs + 1
Write-Host "`n=== 启动计时：第 1 次 + $Runs 次热启动 ===" -ForegroundColor Cyan
for ($i = 0; $i -lt $count; $i++) {
    $tag = if ($i -eq 0) { 'first' } else { "warm$i" }
    $s = Start-CapOnce -TimeoutSec $TimeoutSec
    Wait-Phase -Proc $s.Proc -Phase 'deferred_enum_done' -TimeoutSec 20 | Out-Null
    Start-Sleep -Milliseconds 250
    $mods = Get-FfmpegModules -ProcessId $s.Proc.Id
    $s.Proc.Kill() | Out-Null
    Start-Sleep -Milliseconds 350

    $ph = [ordered]@{}
    foreach ($k in $s.Phases.Keys) { $ph[$k] = $s.Phases[$k] }
    $runs += [ordered]@{
        tag             = $tag
        window_ms       = if ($null -ne $s.WindowMs) { [Math]::Round($s.WindowMs, 1) } else { $null }
        first_frame_t   = if ($ph.Contains('ui_first_frame_done')) { $ph['ui_first_frame_done'].t } else { $null }
        first_frame_dur = if ($ph.Contains('ui_first_frame_done')) { $ph['ui_first_frame_done'].dur } else { $null }
        phases          = $ph
        ffmpeg_modules  = @($mods)
    }
    Write-Host ("  [{0}] 窗口 {1} ms / 首帧结束 t={2} ms / av* 模块 {3}" -f `
        $tag, $runs[-1].window_ms, $runs[-1].first_frame_t,
        $(if ($mods.Count) { $mods -join ',' } else { '(空)' }))
}

# ---------------------------------------------------------------------------
# 3) 汇总（每个阶段的 t 与 dur 的 min/median/max）
# ---------------------------------------------------------------------------
$allPhases = @()
foreach ($r in $runs) { $allPhases += $r.phases.Keys }
$allPhases = $allPhases | Sort-Object -Unique

function Get-Stat {
    param([double[]]$Values)
    if (-not $Values -or $Values.Count -eq 0) { return $null }
    $sorted = $Values | Sort-Object
    $n = $sorted.Count
    $med = if ($n % 2) { $sorted[[int]($n / 2)] } else { ($sorted[$n / 2 - 1] + $sorted[$n / 2]) / 2 }
    return [ordered]@{ min = [Math]::Round($sorted[0], 1); median = [Math]::Round($med, 1); max = [Math]::Round($sorted[-1], 1) }
}

$summary = [ordered]@{}
foreach ($p in $allPhases) {
    $ts = @(); $ds = @()
    foreach ($r in $runs) {
        if ($r.phases.Contains($p)) { $ts += $r.phases[$p].t; $ds += $r.phases[$p].dur }
    }
    $summary[$p] = [ordered]@{ t = (Get-Stat $ts); dur = (Get-Stat $ds); n = $ts.Count }
}

$result = [ordered]@{
    label        = $Label
    exe          = $Exe
    exe_bytes    = (Get-Item $Exe).Length
    exe_written  = (Get-Item $Exe).LastWriteTime.ToString('s')
    runs         = $runs
    summary      = $summary
    modules      = $moduleResult
    note         = 'first = 本次会话第一次运行（非真正冷启动：本机无法清空系统文件缓存）；warm* = 随后运行'
}
$jsonPath = Join-Path $OutDir "$Label.json"
$result | ConvertTo-Json -Depth 8 | Set-Content -Path $jsonPath -Encoding UTF8

Write-Host "`n=== 阶段汇总（t / dur，单位 ms）===" -ForegroundColor Cyan
Write-Host ('{0,-28} {1,>7} {2,>7} {3,>7}   {4,>7} {5,>7} {6,>7}' -f 'phase', 't_min', 't_med', 't_max', 'd_min', 'd_med', 'd_max')
foreach ($p in $allPhases) {
    $v = $summary[$p]
    Write-Host ('{0,-28} {1,7} {2,7} {3,7}   {4,7} {5,7} {6,7}' -f $p,
        $v.t.min, $v.t.median, $v.t.max, $v.dur.min, $v.dur.median, $v.dur.max)
}
Write-Host "`nJSON: $jsonPath" -ForegroundColor Green
if ($Json) { $result | ConvertTo-Json -Depth 8 }
