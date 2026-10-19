# 资源占用采样：启动 CapLite，按固定间隔采样 CPU / 工作集 / 私有字节 / 句柄 / 线程，
# 用来回答"空闲时到底占多少、有没有泄漏、有没有空转重绘"。
#
# 用法（在 CapLite 根目录）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\resource-sampler.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\resource-sampler.ps1 -Seconds 30 -IntervalMs 500
#
# 口径说明（别误读）：
#   * CPU% 是"占**一个核**的百分比"（= ΔTotalProcessorTime / Δ墙钟）；
#     空闲不重绘时应该接近 0；录制中 200ms 一次重绘，也应远低于 100；
#   * WorkingSet 是物理内存（会被系统回收），Priv 是提交量（更接近"真实占用"）；
#   * 句柄/线程数持续单调上涨 = 泄漏信号；启动后有个平台期是正常的。
#
# 为什么用 ProcessStartInfo 而不是 Start-Process：后者走 ShellExecuteEx，
# 会触发附件/发布者检查（见 docs/DEV-SIGNING.md §2.3），自动化里会卡住。

[CmdletBinding()]
param(
    [string]$Exe = '',
    [int]$Seconds = 15,
    [int]$IntervalMs = 500,
    [string]$Label = 'idle',
    [switch]$KeepOpen
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Exe)) { $Exe = Join-Path $Root 'manual-test\CapLite.exe' }
if (-not (Test-Path -LiteralPath $Exe)) { throw "找不到 exe：$Exe" }
$Exe = (Resolve-Path -LiteralPath $Exe).Path
$exeDir = Split-Path -Parent $Exe

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
$psi.WorkingDirectory = $exeDir
$psi.UseShellExecute = $false
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$p = [System.Diagnostics.Process]::Start($psi)

# 等窗口（最多 10s）
$h = 0
while ($sw.ElapsedMilliseconds -lt 10000) {
    $p.Refresh()
    if ($p.MainWindowHandle -ne 0) { $h = $p.MainWindowHandle; break }
    Start-Sleep -Milliseconds 10
}
$windowMs = $sw.ElapsedMilliseconds

$samples = New-Object System.Collections.ArrayList
$lastCpu = $p.TotalProcessorTime
$lastAt = [DateTime]::Now
$cores = [Environment]::ProcessorCount

while ($sw.ElapsedMilliseconds -lt ($Seconds * 1000)) {
    Start-Sleep -Milliseconds $IntervalMs
    $p.Refresh()
    if ($p.HasExited) { break }
    $now = [DateTime]::Now
    $cpu = $p.TotalProcessorTime
    $dt = ($now - $lastAt).TotalSeconds
    $dcpu = ($cpu - $lastCpu).TotalSeconds
    $pct = if ($dt -gt 0) { [math]::Round(100.0 * $dcpu / $dt, 1) } else { 0 }
    $lastCpu = $cpu
    $lastAt = $now
    $null = $samples.Add([pscustomobject]@{
        t_ms    = $sw.ElapsedMilliseconds
        cpu_pct = $pct
        ws_mb   = [math]::Round($p.WorkingSet64 / 1MB, 1)
        priv_mb = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        handles = $p.HandleCount
        threads = $p.Threads.Count
    })
}

"=== 资源采样（$Label）==="
"  exe      : $Exe"
"  窗口可见 : $windowMs ms"
"  逻辑核数 : $cores"
""
$fmt = "{0,8} {1,8} {2,9} {3,9} {4,8} {5,7}"
$fmt -f 't(ms)', 'cpu%', 'WS(MB)', 'Priv(MB)', 'handles', 'threads'
foreach ($s in $samples) {
    $fmt -f $s.t_ms, $s.cpu_pct, $s.ws_mb, $s.priv_mb, $s.handles, $s.threads
}

if ($samples.Count -gt 0) {
    $cpuAvg = [math]::Round(($samples | Measure-Object cpu_pct -Average).Average, 1)
    $cpuMax = [math]::Round(($samples | Measure-Object cpu_pct -Maximum).Maximum, 1)
    $wsMin = [math]::Round(($samples | Measure-Object ws_mb -Minimum).Minimum, 1)
    $wsMax = [math]::Round(($samples | Measure-Object ws_mb -Maximum).Maximum, 1)
    $hMin = ($samples | Measure-Object handles -Minimum).Minimum
    $hMax = ($samples | Measure-Object handles -Maximum).Maximum
    $tMin = ($samples | Measure-Object threads -Minimum).Minimum
    $tMax = ($samples | Measure-Object threads -Maximum).Maximum
    ""
    "=== 汇总 ==="
    "  CPU（占单核）: avg $cpuAvg% / max $cpuMax%"
    "  工作集       : $wsMin .. $wsMax MB"
    "  句柄         : $hMin .. $hMax"
    "  线程         : $tMin .. $tMax"
}

if (-not $KeepOpen) {
    $null = $p.CloseMainWindow()
    Start-Sleep -Milliseconds 800
    if (-not $p.HasExited) { $p.Kill() }
    "  （已关闭）"
} else {
    "  （保持运行，pid = $($p.Id)）"
}
