<#
.SYNOPSIS
  CapLite 端到端回归：录制 → 崩溃恢复 → 关键帧 copy 剪辑导出 → 产物汇总。

.DESCRIPTION
  * 全程只用**随包 LGPL ffmpeg/ffprobe**（默认 CapLite\ffmpeg-n9.0-...\bin，
    可用 -ToolsDir 或 $env:FFMPEG_TOOLS_DIR 指定），绝不用系统 PATH 里的其它 ffmpeg；
    注意这与**构建用**的 `$env:FFMPEG_DIR` 是两件事：后者可以是自建的最小配置包
    （`CapLite\ffmpeg-min`，不含 avdevice/avfilter），但它的 bin 里没有可用的 CLI。
  * 每一步都打印 PASS/FAIL，任一 FAIL 最终 `exit 1`；
  * 不使用 `-ErrorAction SilentlyContinue`：失败时把 stdout/stderr 都打出来；
  * 外部进程用 .NET Process 启动并分离捕获 stdout/stderr —— 这样 stderr 不会被
    Windows PowerShell 5.1 包成 ErrorRecord 噪声，退出码也是进程真实退出码；
  * 环境引导按"环境是否真的就绪"判定（与 run-dev.ps1 保持一致）：只要 FFMPEG_DIR 为空/
    目录不存在，或者 PATH 里找不到 cargo，就按 UTF-8 读出并执行 <工作区>\_rust\env.ps1。
    **不能**只看"PATH 里有没有 cargo"：本机 ~\.cargo\bin 里有 rustup 垫片，旧逻辑会因此
    跳过 env.ps1 → FFMPEG_DIR 为空 → 第 0 步 cargo build 直接失败：`error: FFmpeg not found.`。
    因此本脚本可以在**没有任何 cargo 环境变量**的普通 PowerShell 里直接跑通。

  注意：第 1 步录制用 `--gop 1`（关键帧间隔 1 秒）。原因是第 3 步要导出 [1.2s, 3.4s]，
  而"起点吸附到后一个关键帧、终点吸附到前一个关键帧"在关键帧间隔 2 秒（recorder_smoke
  默认值）时会吸附成 2000..2000，是**空区间**、按规格应当报错。间隔 1 秒时吸附成
  2000..3000，正好能验证吸附 + copy 导出。

.EXAMPLE
  powershell -File scripts/regression.ps1
.EXAMPLE
  # 在另一份 checkout（影子副本）上验证脚本本身
  powershell -File scripts/regression.ps1 -Root ..\_rust\shadow-cap -FfmpegDir ..\CapLite\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0 -ToolsDir ..\CapLite\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0\bin
#>
[CmdletBinding()]
param(
    # 仓库根（默认本脚本的上一级目录）
    [string]$Root = '',
    # 随包 ffmpeg 目录（默认 <Root>\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0）
    [string]$FfmpegDir = '',
    # ffmpeg/ffprobe **CLI** 所在目录（默认取 $env:FFMPEG_TOOLS_DIR；为空则回落到 <FfmpegDir>\bin）
    #
    # 为什么要单列：`$FfmpegDir` 同时是"CLI 来源"。而 `FFMPEG_DIR`（构建用开发包）一旦指向自建的
    # 最小配置包（`CapLite\ffmpeg-min`，--disable-avdevice --disable-avfilter），同一目录里的
    # ffprobe/ffmpeg 就起不来（静态导入 avfilter-12/avdevice-63；缺 → 0xC0000135，
    # 混搭 → 0xC0000139），本脚本会真调它们约 11 次（含 `-af volumedetect`）。
    # 所以 `_rust\env.ps1` 把"CLI 目录"独立成 `$env:FFMPEG_TOOLS_DIR`（见那里的长注释），
    # 这里显式优先用它 —— 这样"构建用哪个 FFmpeg"与"交叉校验用哪个 CLI"可以各自独立。
    [string]$ToolsDir = '',
    # 产物目录（默认 <工作区>\_rust\regression-out）
    [string]$OutDir = '',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Continue'

# ---------------------------------------------------------------------------
# 基础：路径、环境
# ---------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($Root)) {
    $Root = Split-Path -Parent $PSScriptRoot
}
$Root = (Resolve-Path -LiteralPath $Root).Path
$Workspace = Split-Path -Parent $Root

if ([string]::IsNullOrWhiteSpace($FfmpegDir)) {
    $FfmpegDir = Join-Path $Root 'ffmpeg-n9.0-latest-win64-lgpl-shared-9.0'
}
if (-not (Test-Path -LiteralPath $FfmpegDir)) {
    Write-Host "找不到随包 ffmpeg 目录：$FfmpegDir" -ForegroundColor Red
    exit 1
}
# CLI 目录的优先级：显式 -ToolsDir > $env:FFMPEG_TOOLS_DIR（env.ps1 设的）> <FfmpegDir>\bin
$toolsDirSource = '-ToolsDir 参数'
if ([string]::IsNullOrWhiteSpace($ToolsDir)) {
    if (-not [string]::IsNullOrWhiteSpace($env:FFMPEG_TOOLS_DIR)) {
        $ToolsDir = $env:FFMPEG_TOOLS_DIR
        $toolsDirSource = '环境变量 FFMPEG_TOOLS_DIR'
    } else {
        $ToolsDir = Join-Path $FfmpegDir 'bin'
        $toolsDirSource = '回落 <FfmpegDir>\bin'
    }
}
$FFmpeg = Join-Path $ToolsDir 'ffmpeg.exe'
$FFprobe = Join-Path $ToolsDir 'ffprobe.exe'
if (-not (Test-Path -LiteralPath $FFmpeg) -or -not (Test-Path -LiteralPath $FFprobe)) {
    Write-Host "ffmpeg/ffprobe 不完整：$ToolsDir" -ForegroundColor Red
    exit 1
}
$ToolsDir = (Resolve-Path -LiteralPath $ToolsDir).Path
$FFmpeg = Join-Path $ToolsDir 'ffmpeg.exe'
$FFprobe = Join-Path $ToolsDir 'ffprobe.exe'
$script:ToolsDirSource = $toolsDirSource

# 导入构建环境（RUSTUP/CARGO/MSVC/FFMPEG_DIR）。
#
# 环境就绪判定（一次性，与 scripts\run-dev.ps1 保持一致）：
#   * FFMPEG_DIR 非空且目录存在 —— ffmpeg-sys-next 只认它（本机既无 pkg-config 也无 vcpkg）；
#   * PATH 里能找到 cargo。
# 两项都满足才跳过 env.ps1。**不能**只看"PATH 里有没有 cargo"：本机 ~\.cargo\bin 里有 rustup
# 垫片，只看 cargo 会跳过 env.ps1 → FFMPEG_DIR 为空 → 第 0 步 cargo build 直接失败：
#   `error: FFmpeg not found.`
# env.ps1 本身是幂等的（只赋 $env: 变量 + 覆盖式导入 vcvars 环境），这里仍用一次性判定，
# 免得每次运行都重导一遍 MSVC 环境、反复往 PATH 前面拼目录。
#
# 注意：`_rust\env.ps1` 是 **UTF-8 无 BOM** 且含中文注释，Windows PowerShell 5.1 会按
# ANSI 代码页读它而导致解析失败，所以这里显式按 UTF-8 读取文本再 Invoke-Expression，
# 而不是直接 `. $envScript`（那个文件不属于本项目，不动它）。
$envScript = Join-Path $Workspace '_rust\env.ps1'
$cargoCmd = $null
try { $cargoCmd = Get-Command cargo -ErrorAction Stop } catch { $cargoCmd = $null }
$ffmpegDirOk = (-not [string]::IsNullOrWhiteSpace($env:FFMPEG_DIR)) -and (Test-Path -LiteralPath $env:FFMPEG_DIR)

if ((-not $ffmpegDirOk) -or ($null -eq $cargoCmd)) {
    if (Test-Path -LiteralPath $envScript) {
        $why = @()
        if (-not $ffmpegDirOk) { $why += ("FFMPEG_DIR 未设置或目录不存在（当前：'{0}'）" -f $env:FFMPEG_DIR) }
        if ($null -eq $cargoCmd) { $why += 'PATH 里没有 cargo' }
        Write-Host ("[regression] 环境未就绪（{0}），导入构建环境：{1}" -f ($why -join '；'), $envScript)
        $prevEap = $ErrorActionPreference
        $envText = Get-Content -LiteralPath $envScript -Raw -Encoding UTF8
        Invoke-Expression $envText
        $ErrorActionPreference = $prevEap
        $cargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
        if (-not [string]::IsNullOrWhiteSpace($env:FFMPEG_DIR)) {
            Write-Host ("[regression] 已引导环境（FFMPEG_DIR={0}）" -f $env:FFMPEG_DIR) -ForegroundColor Green
        } else {
            Write-Host '[regression] 已执行 env.ps1，但 FFMPEG_DIR 仍为空（随包 FFmpeg 开发包缺失？）' -ForegroundColor Yellow
        }
    } else {
        Write-Host "[regression] 环境未就绪，且找不到 $envScript" -ForegroundColor Yellow
    }
} else {
    Write-Host ("[regression] 环境已就绪（FFMPEG_DIR={0}，cargo={1}）" -f $env:FFMPEG_DIR, $cargoCmd.Source)
}
# 与本次开发保持一致：构建产物统一放 _rust\target-cap（env.ps1 里默认是 _rust\target）
$env:CARGO_TARGET_DIR = Join-Path $Workspace '_rust\target-cap'

if ([string]::IsNullOrWhiteSpace($OutDir)) {
    $OutDir = Join-Path $Workspace '_rust\regression-out'
}
if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Path $OutDir -Force | Out-Null }
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$script:Failures = @()
$script:Summary = @()

# ---------------------------------------------------------------------------
# 工具函数
# ---------------------------------------------------------------------------

function Write-Result {
    param([string]$Name, [bool]$Ok, [string]$Detail = '')
    $suffix = ''
    if (-not [string]::IsNullOrWhiteSpace($Detail)) { $suffix = " — $Detail" }
    if ($Ok) {
        Write-Host ("PASS  {0}{1}" -f $Name, $suffix) -ForegroundColor Green
    } else {
        Write-Host ("FAIL  {0}{1}" -f $Name, $suffix) -ForegroundColor Red
        $script:Failures += $Name
    }
}

# 运行外部命令：分离捕获 stdout / stderr，返回进程真实退出码。
# 为什么不用 `& exe 2>&1 | Out-String`：Windows PowerShell 5.1 会把子进程的 stderr
# 包成 ErrorRecord 混进管道，既污染解析又可能拿不到真实退出码。
function Invoke-Tool {
    param([string]$Exe, [string[]]$Arguments = @())
    $resolved = $null
    if ($Exe -match '[\\/]') {
        if (-not (Test-Path -LiteralPath $Exe)) {
            return [pscustomobject]@{ Code = 9009; Out = ''; Err = "找不到可执行文件：$Exe" }
        }
        $resolved = (Resolve-Path -LiteralPath $Exe).Path
    } else {
        $cmd = $null
        try { $cmd = Get-Command $Exe -ErrorAction Stop } catch { $cmd = $null }
        if ($null -eq $cmd) {
            return [pscustomobject]@{ Code = 9009; Out = ''; Err = "PATH 里找不到命令：$Exe" }
        }
        $resolved = $cmd.Source
    }

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $resolved
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.WorkingDirectory = (Get-Location).Path
    $quoted = @()
    foreach ($a in $Arguments) {
        $s = [string]$a
        if ($s -match '[\s"]') { $s = '"' + ($s -replace '"', '\"') + '"' }
        $quoted += $s
    }
    $psi.Arguments = ($quoted -join ' ')

    try {
        $proc = New-Object System.Diagnostics.Process
        $proc.StartInfo = $psi
        if (-not $proc.Start()) {
            return [pscustomobject]@{ Code = 9009; Out = ''; Err = "无法启动进程：$resolved" }
        }
        $outTask = $proc.StandardOutput.ReadToEndAsync()
        $errTask = $proc.StandardError.ReadToEndAsync()
        $proc.WaitForExit()
        return [pscustomobject]@{
            Code = [int]$proc.ExitCode
            Out  = [string]$outTask.Result
            Err  = [string]$errTask.Result
        }
    } catch {
        return [pscustomobject]@{ Code = 9009; Out = ''; Err = "启动 $resolved 失败：$($_.Exception.Message)" }
    }
}

function Show-Text {
    param([string]$Text, [int]$Lines = 12, [string]$Prefix = '      | ')
    if ([string]::IsNullOrWhiteSpace($Text)) { return }
    $all = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
    $start = [Math]::Max(0, $all.Count - $Lines)
    for ($i = $start; $i -lt $all.Count; $i++) { Write-Host ($Prefix + $all[$i]) }
}

function Show-FailureText {
    param($Result, [int]$Lines = 25)
    if (-not [string]::IsNullOrWhiteSpace($Result.Out)) {
        Write-Host '      --- stdout ---'
        Show-Text $Result.Out $Lines
    }
    if (-not [string]::IsNullOrWhiteSpace($Result.Err)) {
        Write-Host '      --- stderr ---'
        Show-Text $Result.Err $Lines
    }
}

function Get-VideoResolution {
    param([string]$Path)
    $r = Invoke-Tool $FFprobe @('-v', 'error', '-select_streams', 'v:0',
        '-show_entries', 'stream=width,height', '-of', 'csv=p=0', $Path)
    if ($r.Code -ne 0) { return 'ffprobe失败' }
    $lines = @($r.Out -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
    if ($lines.Count -eq 0) { return '?' }
    return $lines[0].Trim()
}

function Get-DurationMs {
    param([string]$Path)
    $r = Invoke-Tool $FFprobe @('-v', 'error', '-show_entries', 'format=duration',
        '-of', 'csv=p=0', $Path)
    $sec = $r.Out.Trim() -as [double]
    if ($null -eq $sec) { return -1 }
    return [int]($sec * 1000 + 0.5)
}

function Get-KeyframeCount {
    param([string]$Path)
    $r = Invoke-Tool $FFprobe @('-v', 'error', '-select_streams', 'v:0',
        '-show_entries', 'packet=flags', '-of', 'csv=p=0', $Path)
    if ($r.Code -ne 0) { return -1 }
    $n = 0
    foreach ($line in ($r.Out -split "`r?`n")) { if ($line -match 'K') { $n++ } }
    return $n
}

function Get-StreamTable {
    param([string]$Path)
    $r = Invoke-Tool $FFprobe @('-v', 'error', '-show_entries', 'stream=codec_type,codec_name',
        '-of', 'csv=p=0', $Path)
    return @($r.Out -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
}

# 从 recorder_smoke 的 **纯 ASCII** KEY=VALUE 行里取值（见 src/bin/recorder_smoke.rs）。
#
# 为什么不解析 `[rec] 统计：…音频 N 帧…`：那是 UTF-8 中文，而 Windows PowerShell 5.1
# 用 `Process.StandardOutput.ReadToEnd()` 读管道时按 **[Console]::OutputEncoding** 解码，
# 该编码**继承自父进程**；父进程若是「chcp 65001 + OutputEncoding=gb2312」的组合，
# 子进程的 UTF-8 字节就被按 GBK 解成乱码（「音频」→「闊抽」），中文正则永远不匹配 →
# 第 8 步被误判成"0 帧/未产出音轨"。KEY=VALUE 是纯 ASCII，任何代码页下都稳定。
function Get-SmokeNumber {
    param([string]$Text, [string]$Key)
    if ([string]::IsNullOrEmpty($Text)) { return $null }
    if ($Text -match ("(?m)^" + [regex]::Escape($Key) + "=(\d+)\s*$")) { return [int]$Matches[1] }
    return $null
}

# 与 `Get-SmokeNumber` 同源，唯一区别是**把"取不到值"和"值就是 0"分开**：
# 返回 `$null` = 输出里根本没有这个 KEY（或值不是十进制整数）；返回 `Value=0` = 真的报了 0。
# 第 5 步的 `REC_WALL_MS` 必须用它：`Get-SmokeNumber` 对 "没输出" 与 "输出 0" 都返回 $null，
# 而 0 ms 是一个"明显不可能"的值，两者要给出不同的失败说明。
# 第 8 步的 `REC_AUDIO_FRAMES` 继续用 `Get-SmokeNumber`（旧语义不动，且那边的 0 也当失败）。
function Get-SmokeNumberOrNull {
    param([string]$Text, [string]$Key)
    if ([string]::IsNullOrEmpty($Text)) { return $null }
    if ($Text -notmatch ("(?m)^" + [regex]::Escape($Key) + "=(.*?)\s*$")) { return $null }
    $raw = $Matches[1]
    if ($raw -notmatch '^\d+$') { return $null }
    return [pscustomobject]@{ Has = $true; Value = [int]$raw }
}

function Get-SmokeValue {
    param([string]$Text, [string]$Key)
    if ([string]::IsNullOrEmpty($Text)) { return $null }
    if ($Text -match ("(?m)^" + [regex]::Escape($Key) + "=(.*?)\s*$")) { return $Matches[1] }
    return $null
}

# 音轨里**真的有数据**吗？用 ffprobe 的**结构化**流信息，不解析任何自然语言。
# 返回：包数（>=0）或 -1（取不到值）。
# 关于 -1：**取不到值不等于 0 帧**。调用方必须把 -1 当成"探测失败/环境不支持"处理
# （明确报错或 SKIP），绝不能把它当断言值去比 `> 0` —— 那会把"探测失败"伪装成"无声"，
# 正是第 8 步之前误报的形态。
function Get-AudioPacketCount {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) { return -1 }
    # AAC 在 MP4 里一包 = 一个 1024 采样的帧，包数直接对应"有多少帧音频被写进文件"。
    $r = Invoke-Tool $FFprobe @('-v', 'error', '-count_packets', '-select_streams', 'a:0',
        '-show_entries', 'stream=nb_read_packets', '-of', 'csv=p=0', $Path)
    if ($r.Code -ne 0) { return -1 }
    $first = @($r.Out -split "`r?`n" | Where-Object { $_.Trim() -ne '' })[0]
    if ($null -eq $first) { return -1 }
    if ($first.Trim() -match '^\d+$') { return [int]$first.Trim() }
    return -1
}

function Add-SummaryRow {
    param([string]$Role, [string]$Path)
    $ok = Test-Path -LiteralPath $Path
    $bytes = 0
    if ($ok) { $bytes = (Get-Item -LiteralPath $Path).Length }
    $streams = @()
    if ($ok) { $streams = Get-StreamTable $Path }
    $res = '?'
    if ($ok) { $res = Get-VideoResolution $Path }
    $ms = -1
    if ($ok) { $ms = Get-DurationMs $Path }
    $kf = -1
    if ($ok) { $kf = Get-KeyframeCount $Path }
    $script:Summary += [pscustomobject]@{
        Role       = $Role
        File       = Split-Path -Leaf $Path
        Exists     = $ok
        Duration   = $ms
        Resolution = $res
        Keyframes  = $kf
        Streams    = ($streams -join ' + ')
        Bytes      = $bytes
    }
}

# 汇总表：对象属性用 ASCII，表头显示中文，避免 PS 5.1 的编码/标识符坑
function Format-Summary {
    $script:Summary | Select-Object `
        @{ n = '角色'; e = { $_.Role } },
        @{ n = '文件'; e = { $_.File } },
        @{ n = '存在'; e = { $_.Exists } },
        @{ n = '时长ms'; e = { $_.Duration } },
        @{ n = '分辨率'; e = { $_.Resolution } },
        @{ n = '关键帧数'; e = { $_.Keyframes } },
        @{ n = '流'; e = { $_.Streams } },
        @{ n = '字节'; e = { $_.Bytes } } |
        Format-Table -AutoSize | Out-String -Width 220
}

Write-Host ''
Write-Host '================ CapLite 端到端回归 ================' -ForegroundColor Cyan
Write-Host "仓库根            : $Root"
Write-Host "产物目录          : $OutDir"
Write-Host "ffmpeg            : $FFmpeg"
Write-Host "ffprobe           : $FFprobe"
Write-Host "CLI 目录来源       : $script:ToolsDirSource"
Write-Host "CARGO_TARGET_DIR  : $env:CARGO_TARGET_DIR"
Write-Host ''

# ---------------------------------------------------------------------------
# 0. 构建需要的两个 smoke 可执行文件
# ---------------------------------------------------------------------------
$RecSmoke = Join-Path $env:CARGO_TARGET_DIR 'debug\recorder_smoke.exe'
$ClipSmoke = Join-Path $env:CARGO_TARGET_DIR 'debug\clip_smoke.exe'

if (-not $SkipBuild) {
    Write-Host '--- 0. cargo build --bins ---'
    $b = Invoke-Tool 'cargo' @('build', '--bins', '--manifest-path', (Join-Path $Root 'Cargo.toml'))
    if ($b.Code -ne 0) { Show-FailureText $b 40 }
    Write-Result '0. cargo build --bins' ($b.Code -eq 0) "退出码 $($b.Code)"
    if ($b.Code -ne 0) {
        Write-Host '构建失败，后续步骤无法进行。' -ForegroundColor Red
        # 与 run-dev.ps1 一致的可操作指引：只在构建输出真的报了 FFmpeg not found 时打印。
        # 这**不改变**通过/失败语义 —— 下面仍然是 FAIL（Write-Result 已记入 Failures）+ exit 1。
        if (("$($b.Out)`n$($b.Err)") -match 'FFmpeg not found') {
            Write-Host ''
            Write-Host "[regression] 构建失败：未找到 FFmpeg。请先在本窗口执行  . '$envScript'  再重试（或加 -SkipBuild 跳过第 0 步构建）。" -ForegroundColor Yellow
        }
        Write-Host ("回归失败：{0} 项" -f $script:Failures.Count) -ForegroundColor Red
        exit 1
    }
} else {
    Write-Host '--- 0. 跳过构建（-SkipBuild）---'
}

$exeOk = $true
if (-not (Test-Path -LiteralPath $RecSmoke)) {
    Write-Result '0b. recorder_smoke.exe 存在' $false $RecSmoke
    $exeOk = $false
}
if (-not (Test-Path -LiteralPath $ClipSmoke)) {
    Write-Result '0c. clip_smoke.exe 存在' $false $ClipSmoke
    $exeOk = $false
}
if (-not $exeOk) {
    Write-Host ("回归失败：{0} 项" -f $script:Failures.Count) -ForegroundColor Red
    exit 1
}
Write-Result '0b. smoke 可执行文件就绪' $true 'recorder_smoke.exe / clip_smoke.exe'

# 运行期 DLL 自检：链接好的 exe 必须能在同级目录找到 FFmpeg/openh264 运行库
$exeDir = Split-Path -Parent $RecSmoke
$dllNames = @()
foreach ($d in @(Get-ChildItem -LiteralPath $exeDir -Filter '*.dll')) { $dllNames += $d.Name }
$dllOk = (@($dllNames | Where-Object { $_ -like 'avcodec-*' }).Count -gt 0) -and
         (@($dllNames | Where-Object { $_ -like 'avformat-*' }).Count -gt 0) -and
         (@($dllNames | Where-Object { $_ -like 'avutil-*' }).Count -gt 0) -and
         (@($dllNames | Where-Object { $_ -like 'openh264*' }).Count -gt 0)
Write-Result '0c. 运行期 DLL 就位（同 exe 目录）' $dllOk ($dllNames -join ', ')

# 进程自检：clip_smoke 不带参数应打印用法并以退出码 2 结束，用来确认 DLL 真的能加载
$preflight = Invoke-Tool $ClipSmoke @()
Write-Result '0d. clip_smoke.exe 能启动（用法提示 + 退出码 2）' ($preflight.Code -eq 2) "退出码 $($preflight.Code)"
if ($preflight.Code -ne 2) { Show-FailureText $preflight 10 }

# ---------------------------------------------------------------------------
# 1. 正常录制 5 秒（画面 + 声音）
#
# **目录分工（新）**：`recorder_smoke <秒> <导出.mp4>` 里那个路径是**导出产物**的落点；
# 流文件（分片 MP4）写到**同级的 `Cache\`** 下，与导出文件**同名同基名**。
# 停止时导出成功**也不再删除流文件**（用户最终要求：不做任何删除，留着可找回）。
# 所以这一步的断言是：
#   ① 导出目录里有普通 MP4（且非空、h264+aac 双流）
#   ② **Cache 里的流文件仍然存在**（这条覆盖了旧的"导出成功后分片已删除"断言）
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '--- 1. 录制 5 秒（画面+声音，关键帧间隔 1s；导出到输出目录 + 流文件留在 Cache）---'
$export1 = Join-Path $OutDir 'reg1.mp4'
$cache1 = Join-Path (Join-Path $OutDir 'Cache') 'reg1.mp4'
foreach ($f in @($export1, $cache1)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }

$r1 = Invoke-Tool $RecSmoke @('5', $export1, '--gop', '1')
Show-Text $r1.Out 8

$export1Exists = (Test-Path -LiteralPath $export1) -and ((Get-Item -LiteralPath $export1).Length -gt 0)
# **Cache 里的流文件必须保留**（不做任何删除；这是"导出失败也能找回"的依据）
$cache1Kept = (Test-Path -LiteralPath $cache1) -and ((Get-Item -LiteralPath $cache1).Length -gt 0)
$streams1 = @()
if (Test-Path -LiteralPath $export1) { $streams1 = Get-StreamTable $export1 }
$joined1 = $streams1 -join ' '
# ffprobe 的 csv 列顺序按结构体而不是我们请求的顺序输出（实际是 "h264,video"），
# 所以这里按"包含关系"判断，别写死顺序
$hasVideo = ($joined1 -match 'h264') -and ($joined1 -match 'video')
$hasAudio = ($joined1 -match 'aac') -and ($joined1 -match 'audio')
$twoStreams = ($streams1.Count -eq 2)
# 音频码率**不在这里断言**：AAC 对"静音"几乎不产生字节（本机静音环回实测只有 ~2.3 kbps），
# 拿实测码率去比"192 kbps"会误报。内部固定 192 kbps 由单测覆盖
#（config::tests::audio_bitrate_and_keyframe_interval_are_normalized +
#  settings::tests::sanitize_is_applied_before_save_pipeline），这里只确认音轨是 aac。
$step1Ok = ($r1.Code -eq 0) -and $export1Exists -and $cache1Kept -and $hasVideo `
    -and $hasAudio -and $twoStreams
Write-Result '1. 录制 5 秒：导出 MP4 就绪 + Cache 流文件保留 + h264/aac 双流' $step1Ok (
    "退出码 $($r1.Code)；流=[$joined1]；导出=$export1Exists Cache保留=$cache1Kept")
if (-not $step1Ok) { Show-FailureText $r1 30 }

# ---------------------------------------------------------------------------
# 2. 崩溃恢复：--crash 会 abort（非 0 退出是**预期**）
#
# 崩溃时进程从未走到"导出"，所以：
#   ① **Cache 里必须有流文件**（它从录制开始就在往那里写分片）且能解码；
#   ② 导出目录里**不应**出现同名普通 MP4。
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '--- 2. 崩溃恢复（recorder_smoke --crash，非 0 退出是预期；Cache 里的流文件必须可解）---'
$export2 = Join-Path $OutDir 'reg2.mp4'
$cache2 = Join-Path (Join-Path $OutDir 'Cache') 'reg2.mp4'
foreach ($f in @($export2, $cache2)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }

$r2 = Invoke-Tool $RecSmoke @('5', $export2, '--gop', '1', '--crash')
Write-Host ("      进程退出码 = {0}（abort，非 0 属预期）" -f $r2.Code)
Show-Text $r2.Out 5

$exists2 = Test-Path -LiteralPath $cache2
$noExport2 = -not (Test-Path -LiteralPath $export2)
$decode2 = Invoke-Tool $FFmpeg @('-v', 'error', '-i', $cache2, '-f', 'null', '-')
$decode2Clean = ($decode2.Code -eq 0) -and [string]::IsNullOrWhiteSpace($decode2.Err)
$streams2 = @()
if ($exists2) { $streams2 = Get-StreamTable $cache2 }
$step2Ok = $exists2 -and $noExport2 -and $decode2Clean -and ($streams2.Count -ge 1)
Write-Result '2. 崩溃后 Cache 里的流文件完整可解，且导出目录没有同名文件' $step2Ok (
    "Cache 存在=$exists2；导出目录无同名文件=$noExport2；ffmpeg -f null - 干净=$decode2Clean；退出码 $($r2.Code)")
if (-not $step2Ok) { Show-FailureText $decode2 30 }

# ---------------------------------------------------------------------------
# 3. 剪辑导出：对**导出产物** reg1.mp4 做 copy 剪辑（输入必须是导出的普通 MP4）
# ---------------------------------------------------------------------------
Write-Host ''
# 选点**不再硬编码 [1.2s, 3.4s]**：产品侧关键帧间隔已经是"自动"（内部 2 秒），
# 硬编码的窗口会依赖"1 秒 GOP"这种只存在于本脚本 `--gop 1` 覆盖下的巧合。
# 这里先用 ffprobe 读出**这个文件真实的关键帧位置**，再取"第 1、2 个关键帧之间"与
# "倒数第 2、1 个关键帧之间"的中点：无论 GOP 是 1 秒还是 2 秒，吸附后都必然是
# [k1, k_{n-2}] 这样一个**非空**区间（n ≥ 3 时）。
$kfProbe = Invoke-Tool $FFprobe @('-v', 'error', '-select_streams', 'v:0', '-skip_frame', 'nokey',
    '-show_entries', 'frame=pts_time', '-of', 'csv=p=0', $export1)
$kfTimes = @($kfProbe.Out -split "`r?`n" | Where-Object { $_ -match '^[0-9]' } | ForEach-Object { [double]$_ })
Write-Host ("      实测关键帧 {0} 个：{1}" -f $kfTimes.Count, ($kfTimes -join ', '))
if ($kfTimes.Count -ge 3) {
    $selStartSec = ($kfTimes[0] + $kfTimes[1]) / 2.0
    $selEndSec = ($kfTimes[$kfTimes.Count - 2] + $kfTimes[$kfTimes.Count - 1]) / 2.0
} else {
    # 关键帧太少（<3）：退化成"前两个关键帧之间的 1/4 与 3/4 处"，仍然只依赖实测位置
    $span = if ($kfTimes.Count -ge 2) { $kfTimes[1] - $kfTimes[0] } else { 1.0 }
    $base = $kfTimes[0]
    $selStartSec = $base + $span * 0.25
    $selEndSec = $base + $span * 0.75
}
$selStartArg = '{0:0.###}s' -f $selStartSec
$selEndArg = '{0:0.###}s' -f $selEndSec
Write-Host ("--- 3. 关键帧吸附 + copy 剪辑导出 [{0}, {1}]（按实测关键帧选点）---" -f $selStartArg, $selEndArg)
$out3 = Join-Path $OutDir 'reg1-clip.mp4'
if (Test-Path -LiteralPath $out3) { Remove-Item -LiteralPath $out3 -Force }

$r3 = Invoke-Tool $ClipSmoke @($export1, $selStartArg, $selEndArg, $out3)
Show-Text $r3.Out 40
if (-not [string]::IsNullOrWhiteSpace($r3.Err)) { Write-Host '      --- stderr ---'; Show-Text $r3.Err 20 }

$kv = @{}
foreach ($line in ($r3.Out -split "`r?`n")) {
    if ($line -match '^([A-Z_]+)=(.*)$') { $kv[$Matches[1]] = $Matches[2].Trim() }
}
$clipResult = $kv['CLIP_RESULT']
$selMs = -1
if ($kv.ContainsKey('CLIP_SELECTION_MS')) { $selMs = [int]$kv['CLIP_SELECTION_MS'] }
$sameRes = ($kv['CLIP_SAME_RESOLUTION'] -eq 'true')
$bitCopy = ($kv['CLIP_BITSTREAM_COPY'] -eq 'true')
$out3Exists = Test-Path -LiteralPath $out3

$srcRes = Get-VideoResolution $export1
$dstRes = '?'
if ($out3Exists) { $dstRes = Get-VideoResolution $out3 }
$dstMs = -1
if ($out3Exists) { $dstMs = Get-DurationMs $out3 }
$decode3 = Invoke-Tool $FFmpeg @('-v', 'error', '-i', $out3, '-f', 'null', '-')
$decode3Clean = ($decode3.Code -eq 0) -and [string]::IsNullOrWhiteSpace($decode3.Err)

$durDiff = -1
$durOk = $false
if ($selMs -ge 0 -and $dstMs -ge 0) {
    $durDiff = [Math]::Abs($dstMs - $selMs)
    $durOk = ($durDiff -le 250)
}
Write-Host ("      吸附后区间 = {0} → {1}（{2} ms）；ffprobe 读出产物时长 = {3} ms（差 {4} ms）" -f `
    $kv['CLIP_SNAP_START_MS'], $kv['CLIP_SNAP_END_MS'], $selMs, $dstMs, $durDiff)
Write-Host ("      分辨率：源 $srcRes / 产物 $dstRes；位流拷贝(未重新编码)=$bitCopy")

$step3Ok = ($r3.Code -eq 0) -and ($clipResult -eq 'PASS') -and $out3Exists -and $sameRes `
    -and $bitCopy -and ($srcRes -eq $dstRes) -and $durOk -and $decode3Clean
Write-Result '3. copy 导出：分辨率一致 / 时长正确 / 解码干净' $step3Ok (
    "退出码 $($r3.Code)；结果=$clipResult；分辨率 $srcRes→$dstRes；时长差=$durDiff ms；解码干净=$decode3Clean")
if (-not $step3Ok) { Show-FailureText $decode3 20 }

# ---------------------------------------------------------------------------
# 4. 汇总所有产物的关键信息
#
# 注意：**正常录制的 Cache 流文件也要汇总** —— 现行语义是"不删除"：停止并成功导出后
# Cache 里的 `reg1.mp4` **仍然在**（与导出产物同名同基名，只是分处两个目录）。
# 汇总表只列"此刻应当存在"的产物，逐个断言可被 ffprobe 解析。
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '--- 4. 产物汇总 ---'
Write-Host '      （① 正常录制：导出产物在输出目录，Cache 里的流文件同名同基名且被保留=不删除；② 崩溃场景的流文件同样保留在 Cache）'
Add-SummaryRow '① 正常录制（输出目录里的导出 MP4）' $export1
Add-SummaryRow '① 正常录制（Cache 里保留的流文件）' $cache1
Add-SummaryRow '② 崩溃后 Cache 里的流文件' $cache2
Add-SummaryRow '③ 剪辑导出（copy）' $out3

Write-Host (Format-Summary)

Write-Host '--- 4b. ffprobe 明细 ---'
foreach ($row in $script:Summary) {
    $p = Join-Path $OutDir $row.File
    if (-not (Test-Path -LiteralPath $p)) { continue }
    Write-Host ("[{0}] {1}" -f $row.Role, $p)
    $d = Invoke-Tool $FFprobe @('-v', 'error',
        '-show_entries', 'format=format_name,duration,size,bit_rate',
        '-show_entries', 'stream=index,codec_type,codec_name,width,height,avg_frame_rate,sample_rate,channels',
        '-of', 'default=noprint_wrappers=1', $p)
    Show-Text $d.Out 40 '      '
    Write-Host ''
}
$step4Ok = $true
foreach ($row in $script:Summary) {
    if (-not $row.Exists) { $step4Ok = $false }
}
Write-Result '4. 所有产物均可被 ffprobe 解析' $step4Ok "$($script:Summary.Count) 个产物"

# ---------------------------------------------------------------------------
# 5~8. 音频链路：麦克风 / 系统声音（含"真有声"与"无声必须报错"两条）
#
# 背景：系统声音这条链早就实测可用，麦克风是**唯一只有人工验证**的分支。
# 这几步把它自动化：
#   5. 仅麦克风 → 必须有 aac 音轨 + 时长≈录制时长（**不断言 RMS 下限**：真实房间
#      底噪可能极低，甚至静音，卡阈值会让回归不稳定；RMS 只作为信息打印）
#   6. 画面 + 麦克风 → h264 + aac 双流
#   7. 系统声音 + 播放 1kHz 合成音 → 断言 RMS 明显高于静音（> 0.05）；
#      播放失败就**跳过**（不算失败）
#   8. 要求声音却一帧都没采到（`--audio none --audio-only`）→ 必须**明确报错并退出非 0**，
#      不允许交给用户一个"没声音的成功文件"
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '--- 5. 仅麦克风 3 秒：aac 音轨 + 时长一致（RMS 仅打印）---'
$out5 = Join-Path $OutDir 'reg5-mic.mp4'
$plain5 = $out5
foreach ($f in @($out5, $plain5)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }
# `$t5`/`$wall5`：**只为诊断**保留的"旧数据源"（脚本自己的墙钟），**不再参与第 5 步判定**。
# 它存在的唯一目的：让同一次运行能直接对比"旧断言会不会误报 FAIL"与"新断言的结果"。
$t5 = Get-Date
$r5 = Invoke-Tool $RecSmoke @('3', $out5, '--audio-only', '--audio', 'mic')
$wall5 = ((Get-Date) - $t5).TotalMilliseconds
# **计时数据源**：recorder_smoke 自报的"录制循环真正开始→停止就绪"墙钟（ASCII KEY=VALUE）。
#   ✗ 不再取 `$t5 = Get-Date` / `((Get-Date) - $t5)` 这种脚本自己的墙钟：它把 exe 冷启动 +
#     设备初始化 + 停止导出全算进"录制时长"。密集运行（cargo build/test 刚结束）时实测
#     3050 ms 的产物配上 3927 / 4127 ms 的脚本墙钟 → 差 982 / 1183 ms → 旧断言（容差 700 ms）
#     偶发 FAIL，而产物时长本身每次都稳在 2944 ms。抖动全部来自被算进去的启动/收尾开销。
#   ✓ REC_WALL_MS 与产物时长是**同一个"真实录制窗口"**的两种度量，才可以直接比。
$recWall5 = Get-SmokeNumberOrNull $r5.Out 'REC_WALL_MS'
Show-Text $r5.Out 12
$exists5 = Test-Path -LiteralPath $plain5
$streams5 = @()
if ($exists5) { $streams5 = Get-StreamTable $plain5 }
$joined5 = $streams5 -join ' '
$hasAac5 = ($joined5 -match 'aac') -and ($joined5 -match 'audio')
# `Get-StreamTable` 只给 codec_type/codec_name，采样率/声道单独问一次
$sr5 = -1
$ch5 = -1
if ($exists5) {
    $p5 = Invoke-Tool $FFprobe @('-v', 'error', '-select_streams', 'a:0',
        '-show_entries', 'stream=sample_rate,channels', '-of', 'csv=p=0', $plain5)
    $parts5 = @($p5.Out.Trim() -split ',')
    if ($parts5.Count -ge 1 -and $parts5[0] -match '^\d+$') { $sr5 = [int]$parts5[0] }
    if ($parts5.Count -ge 2 -and $parts5[1] -match '^\d+$') { $ch5 = [int]$parts5[1] }
}
$srOk5 = ($sr5 -eq 48000)
$dur5 = -1
if ($exists5) { $dur5 = Get-DurationMs $plain5 }
# 时长对齐：**产物音频时长 ≈ recorder_smoke 自报的录制窗口（REC_WALL_MS）**，容差 500 ms。
# 为什么不是脚本墙钟：见上面 `$recWall5` 处的注释（冷启动/设备初始化/导出收尾会把这~100 ms
# 甚至 ~1200 ms 的抖动塞进被比对的量里）。为什么不是 REC_WALL_MS 缺失时退回脚本墙钟：
# 那等于"取不到值就换一把会抖的尺子"，抖动照样偶发 FAIL；按规格，取不到值必须**明确 FAIL**。
# 容差为何是 500 ms 而不是原来那 700 ms：现在两边都是"真实录制窗口"的度量，实测偏差只有
# ~40 ms（AAC 每帧 1024 采样 = 21.3 ms 的量化 + 音频线程"多久读一次设备缓冲"）；
# 500 ms 已经是实测偏差的 10 倍以上余量，同时比旧容差更严。
$dur5DiffMs = -1
$dur5Ok = $false
if (($null -ne $recWall5) -and ($recWall5.Value -gt 0) -and ($dur5 -ge 0)) {
    $dur5DiffMs = [Math]::Abs($dur5 - $recWall5.Value)
    $dur5Ok = ($dur5DiffMs -le 500)
}
$durOk5 = $dur5Ok
# 分得清"取不到值"与"真的不等长"（沿用第 8 步的原则：探测失败 != 不通过的理由被静默掉）
$why5 = @()
if ($null -eq $recWall5) {
    $why5 += '取不到 REC_WALL_MS：recorder_smoke 没有输出这个 KEY=VALUE（二进制过期）或输出无法解析 —— 明确判 FAIL（不回退脚本墙钟）'
} elseif ($recWall5.Value -le 0) {
    $why5 += "REC_WALL_MS=$($recWall5.Value)（必须 > 0）"
}
if ($dur5 -lt 0) { $why5 += '取不到产物音频时长（ffprobe 解析失败）' }
if ($why5.Count -eq 0 -and -not $dur5Ok) {
    $why5 += "产物时长与录制窗口相差 $dur5DiffMs ms（> 500 ms 容差）"
}
# RMS：只打印，不断言（真实麦克风底噪不可控）
$rms5 = ''
if ($exists5) {
    $det = Invoke-Tool $FFmpeg @('-hide_banner', '-i', $plain5, '-af', 'volumedetect', '-f', 'null', '-')
    $rms5 = (@($det.Err -split "`r?`n" | Where-Object { $_ -match 'mean_volume|max_volume' }) -join ' ')
}
Write-Host "      麦克风音轨 RMS（仅信息，不做断言）：$rms5"
# 诊断：旧数据源（脚本墙钟）在本机这次跑出来差多少。**这一行不影响判定**，只为暴露抖动来源：
# 它 > 700 ms（旧容差）时就是旧断言会误报 FAIL 的时刻，而同一行的新数据源仍然很小。
$oldDiff5 = if ($dur5 -ge 0) { [Math]::Abs($dur5 - [int]$wall5) } else { -1 }
$oldNote5 = if ($oldDiff5 -gt 700) { ' → 旧断言（容差 700 ms）在此刻会误报 FAIL' } else { '' }
Write-Host ("      诊断（不影响判定）：脚本墙钟 $([int]$wall5) ms − 产物 $dur5 ms = 差 $oldDiff5 ms$oldNote5")
$step5Ok = ($r5.Code -eq 0) -and $exists5 -and $hasAac5 -and $srOk5 -and $durOk5
$recWall5Text = if ($null -eq $recWall5) { '取不到值(FAIL)' } else { "$($recWall5.Value)" }
$durDiff5Text = if ($null -ne $recWall5) { "$dur5DiffMs ms" } else { '未比对（REC_WALL_MS 取不到值）' }
$chText5 = if ($ch5 -ge 0) { "$ch5 ch" } else { '声道数取不到值' }
Write-Result '5. 仅麦克风：aac 48kHz 音轨 + 时长与录制窗口一致' $step5Ok (
    "退出码 $($r5.Code)；流=[$joined5]；采样率 $sr5 Hz / $chText5；" +
    "产物 $dur5 ms / REC_WALL_MS $recWall5Text ms（差 $durDiff5Text，容差 500 ms）")
if (-not $step5Ok) {
    # 与第 8 步同样的原则：**先讲清楚是哪一种失败**（取不到值 / 真的不等长 / 流不对）
    if ($why5.Count -gt 0) { Write-Host ('      失败原因：' + ($why5 -join '；')) -ForegroundColor Yellow }
    Show-FailureText $r5 20
}

Write-Host ''
Write-Host '--- 6. 画面 + 麦克风 3 秒：h264 + aac 双流 ---'
$out6 = Join-Path $OutDir 'reg6-mic-video.mp4'
$plain6 = $out6
foreach ($f in @($out6, $plain6)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }
$r6 = Invoke-Tool $RecSmoke @('3', $out6, '--audio', 'mic')
Show-Text $r6.Out 10
$exists6 = Test-Path -LiteralPath $plain6
$streams6 = @()
if ($exists6) { $streams6 = Get-StreamTable $plain6 }
$joined6 = $streams6 -join ' '
$hasVideo6 = ($joined6 -match 'h264') -and ($joined6 -match 'video')
$hasAudio6 = ($joined6 -match 'aac') -and ($joined6 -match 'audio')
$step6Ok = ($r6.Code -eq 0) -and $exists6 -and $hasVideo6 -and $hasAudio6
Write-Result '6. 画面+麦克风：h264 + aac 两个流' $step6Ok (
    "退出码 $($r6.Code)；流=[$joined6]")
if (-not $step6Ok) { Show-FailureText $r6 20 }

Write-Host ''
Write-Host '--- 7. 系统声音 + 1kHz 合成音：RMS 必须明显高于静音（播放失败则跳过）---'
$tone = Join-Path (Split-Path -Parent $PSScriptRoot) '_rust\audio\tone1k.wav'
if (-not (Test-Path -LiteralPath $tone)) {
    $tone = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) '_rust\audio\tone1k.wav'
}
$out7 = Join-Path $OutDir 'reg7-tone.mp4'
$plain7 = $out7
foreach ($f in @($out7, $plain7)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }
$toneOk = Test-Path -LiteralPath $tone
$player = $null
if ($toneOk) {
    # 用系统自带的 SoundPlayer 循环播放（不依赖第三方播放器）；失败就跳过这一步。
    # 注意：**不要**在这里 `Add-Type -AssemblyName System.Windows.Extensions` —— 那是
    # .NET Core 的装配件名，Windows PowerShell 5.1 里会抛错（脚本顶部 ErrorActionPreference=Stop，
    # 于是整步被误判成"播放失败"跳过）。`System.Media.SoundPlayer` 在 5.1 的 System.dll 里就有。
    try {
        $player = New-Object System.Media.SoundPlayer $tone
        $player.PlayLooping()
        Start-Sleep -Milliseconds 300
    } catch {
        Write-Host "      （跳过）无法播放合成音：$($_.Exception.Message)"
        $toneOk = $false
    }
}
if (-not $toneOk) {
    Write-Result '7. 系统声音内容可判定（RMS > 0.05）' $true '已跳过：本机没有合成音文件或无法播放（不算失败）'
} else {
    $r7 = Invoke-Tool $RecSmoke @('3', $out7, '--audio', 'system')
    $player.Stop()
    Show-Text $r7.Out 10
    $exists7 = Test-Path -LiteralPath $plain7
    $rmsDb = $null
    if ($exists7) {
        $det = Invoke-Tool $FFmpeg @('-hide_banner', '-i', $plain7, '-af', 'volumedetect', '-f', 'null', '-')
        $line = @($det.Err -split "`r?`n" | Where-Object { $_ -match 'mean_volume' }) -join ''
        if ($line -match 'mean_volume:\s*(-?[0-9.]+)') { $rmsDb = [double]$Matches[1] }
    }
    # dBFS → 线性幅度：0.05 ≈ -26 dB
    $lin = $null
    if ($null -ne $rmsDb) { $lin = [Math]::Pow(10.0, $rmsDb / 20.0) }
    $step7Ok = ($r7.Code -eq 0) -and $exists7 -and ($null -ne $lin) -and ($lin -gt 0.05)
    Write-Result '7. 系统声音内容可判定（RMS > 0.05）' $step7Ok (
        "退出码 $($r7.Code)；mean_volume=$rmsDb dBFS ≈ $([Math]::Round($lin, 4)) 线性")
    if (-not $step7Ok) { Show-FailureText $r7 20 }
}

Write-Host ''
Write-Host '--- 8. 音源全关 → 配置归一为"至少录系统声音"，绝不产出无声文件 ---'
# 产品策略（`Config::sanitized` + 单测 `audio_sources_never_both_off`）：
# 内容模式要求声音时，音源不能全关 —— `sanitized()` 会把 `audio_system` 打开。
# 所以 `--audio none` 这类"音源全关"的输入，正确行为是**配置被归一** → 产物里有 aac 音轨
# **且该音轨确实有数据**，而不是"录出一个没有声音的成功文件"。
#
# ★ 断言的**数据来源**（这里是本步唯一允许的来源）：
#   ① ffprobe 的结构化流信息：`-show_entries stream=codec_type,codec_name`（有 aac/audio 流）
#   ② ffprobe 的**包计数**：`-count_packets -select_streams a:0 -show_entries stream=nb_read_packets`
#      （AAC 在 MP4 里一包 = 一帧；包数 > 0 = 音轨真的有数据）
#   ③ recorder_smoke 的 **纯 ASCII KEY=VALUE**（`REC_AUDIO_FRAMES=`）作为交叉校验
#
# ✗ 绝不解析 `[rec] 统计：…音频 N 帧…`：那是 UTF-8 中文，而 Windows PowerShell 5.1
#   用 `Process.StandardOutput.ReadToEnd()` 读管道时按 **[Console]::OutputEncoding** 解码，
#   该编码**继承自父进程**。父进程是「chcp 65001 + [Console]::OutputEncoding=gb2312」
#   这种组合时（DSH/CI 里很常见），子进程的 UTF-8 字节被按 GBK 解成乱码
#   （「音频」→「闊抽」），中文正则永远匹配不上 → 明明产出了 aac 音轨却被误判成
#   "音频帧=-1 / 没产出音轨"。实测：子进程 Console.OutputEncoding=gb2312 时
#   `'音频\s+(\d+)\s+帧'` match=False；=utf-8 时 match=True。
#
# ✗ 也不允许"解析失败/空值 → 折算成 -1 → 当断言值比 > 0"：取不到值时**明确判 FAIL**，
#   不能把"探测失败"伪装成"没有声音"（反过来也一样不能伪装成 PASS）。
$out8 = Join-Path $OutDir 'reg8-silent.mp4'
$plain8 = $out8
foreach ($f in @($out8, $plain8)) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force } }
$r8 = Invoke-Tool $RecSmoke @('2', $out8, '--audio-only', '--audio', 'none')
Show-Text $r8.Out 10
$exists8 = Test-Path -LiteralPath $plain8
$streams8 = @()
if ($exists8) { $streams8 = Get-StreamTable $plain8 }
$joined8 = $streams8 -join ' '
$hasAac8 = ($joined8 -match 'aac') -and ($joined8 -match 'audio')
# ① 结构化：音轨包数（-1 = 探测失败，不是"0 帧"）
$audioPackets8 = -1
if ($exists8) { $audioPackets8 = Get-AudioPacketCount $plain8 }
# ② 结构化：recorder_smoke 自报的音频帧数（ASCII KEY=VALUE，交叉校验）
$recFrames8 = Get-SmokeNumber $r8.Out 'REC_AUDIO_FRAMES'
# ③ 结构化：本次是否真的请求了音源（归一后 audio_system 必须为 true）+ 音源降级警告
$audioRequested8 = Get-SmokeValue $r8.Out 'REC_AUDIO_REQUESTED'
$audioWarn8 = Get-SmokeValue $r8.Out 'REC_AUDIO_WARNINGS'
$recFrames8Ok = ($null -ne $recFrames8) -and ($recFrames8 -gt 0)
$step8Ok = ($r8.Code -eq 0) -and $exists8 -and $hasAac8 -and ($audioPackets8 -gt 0) `
    -and ($audioRequested8 -eq 'true') -and $recFrames8Ok
$frames8Text = if ($null -eq $recFrames8) { '取不到值(FAIL)' } else { "$recFrames8" }
Write-Result '8. 音源全关时归一为系统声音（有 aac 且音频帧 > 0，不产出无声文件）' $step8Ok (
    "退出码 $($r8.Code)；流=[$joined8]；ffprobe 音轨包数=$audioPackets8；" +
    "smoke REC_AUDIO_FRAMES=$frames8Text（REC_AUDIO_REQUESTED=$audioRequested8；REC_AUDIO_WARNINGS=$audioWarn8）")
if (-not $step8Ok) {
    # "取不到值"与"真的是 0"必须分得开：不许把探测失败伪装成"没有声音"（也不许反过来）。
    $why = @()
    if (-not $exists8) { $why += '导出目录里没有第 8 步的产物（录制/导出失败）' }
    if ($exists8 -and -not $hasAac8) { $why += "产物里没有 aac 音轨（流=[$joined8]）" }
    elseif ($exists8 -and ($audioPackets8 -lt 0)) {
        $why += '探测失败：ffprobe 给不出音轨包数（**不是**"没有声音"；环境/ffprobe 问题）'
    } elseif ($exists8 -and ($audioPackets8 -eq 0)) {
        $why += '音轨存在但包数为 0：真的产出了无声文件'
    }
    if ($audioRequested8 -ne 'true') {
        $why += "REC_AUDIO_REQUESTED=$audioRequested8（归一后系统声音没被打开）"
    }
    if ($null -eq $recFrames8) {
        $why += 'recorder_smoke 没有输出 REC_AUDIO_FRAMES（结构化输出缺失/二进制过期）'
    } elseif ($recFrames8 -le 0) {
        $why += "recorder_smoke 自报音频帧数 = $recFrames8（交叉校验不一致）"
    }
    if ($r8.Code -ne 0) { $why += "recorder_smoke 退出码 $($r8.Code)（非 0）" }
    if ($why.Count -gt 0) { Write-Host ('      失败原因：' + ($why -join '；')) -ForegroundColor Yellow }
    Show-FailureText $r8 20
}

# ---------------------------------------------------------------------------
# 结果
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '================ PASS/FAIL 汇总 ================' -ForegroundColor Cyan
Write-Host (Format-Summary)
if ($script:Failures.Count -eq 0) {
    Write-Host '全部 PASS' -ForegroundColor Green
    exit 0
}
Write-Host ("回归失败：{0} 项 -> {1}" -f $script:Failures.Count, ($script:Failures -join '；')) -ForegroundColor Red
exit 1
