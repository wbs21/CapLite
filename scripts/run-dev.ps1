<#
.SYNOPSIS
  CapLite 一键开发运行：cargo build --bin CapLite → 开发签名（含清 MotW）→ 启动 CapLite.exe。

.DESCRIPTION
  为什么要签名：cargo 每次重新编译都会产出**全新的未签名** exe，Windows 于是会弹
  「无法验证发布者」/ SmartScreen「已保护你的电脑」。本脚本把"构建 → 签名 → 启动"串起来，
  每次跑一次就不会看到确认框。签名细节见 scripts\dev-sign.ps1 与 docs\DEV-SIGNING.md。

  环境引导（按"环境是否真的就绪"判定，与 PATH 里有没有 cargo 无关）：
    * 只要环境还不完整 —— FFMPEG_DIR 为空/指向不存在的目录，或者 PATH 里找不到 cargo ——
      就按 UTF-8 读出并执行 <工作区>\_rust\env.ps1
      （该文件是 UTF-8 无 BOM，Windows PowerShell 5.1 直接 `.` 点源会按 ANSI 读而解析失败，
        所以显式 Get-Content -Encoding UTF8 + Invoke-Expression）；
    * **不能**只看"PATH 里有没有 cargo"：本机 ~\.cargo\bin 里有 rustup 垫片，旧逻辑会因此
      跳过 env.ps1，FFMPEG_DIR/LIBCLANG_PATH/BINDGEN_EXTRA_CLANG_ARGS 全为空，
      cargo build 直接失败：`error: FFmpeg not found.`；
    * 构建产物固定放 <工作区>\_rust\target-cap（与开发时的 CARGO_TARGET_DIR 一致）；
    * 构建后自检 <Bin>.exe 与它旁边的 avcodec/avformat/avutil/swresample/swscale/openh264 DLL。
  因此本脚本可以在**没有任何 cargo 环境变量**的普通 PowerShell 里直接跑通。

  启动方式（重要）：
    默认用 ProcessStartInfo(UseShellExecute = $false) **直接 CreateProcess**，
    不经过 Shell。`Start-Process` 默认 UseShellExecute = $true，会走 ShellExecuteEx 的
    附件/发布者安全检查 ——「无法验证发布者。你确定要运行此软件吗？」这个**模态**确认框
    就长在那条路径上（会卡住自动化）。本脚本默认绕开它；要复现/对比老行为加 -UseShellExecute。

.PARAMETER Root
  仓库根（默认本脚本的上一级目录）。

.PARAMETER Bin
  要构建/启动的 bin 名，默认 cap。

.PARAMETER Release
  构建 release 版本（默认 debug；两者产物目录不同，签名脚本两个目录都会签）。

.PARAMETER AppArgs
  透传给 CapLite.exe 的参数（多个参数请用逗号分隔写成一个字符串）。

.PARAMETER NoBuild
  跳过 cargo build（只签名 + 启动，用于已经构建好的情况）。

.PARAMETER NoStart
  构建 + 签名后不启动。

.PARAMETER UseShellExecute
  用 Start-Process（UseShellExecute = $true）启动，即经过 Shell 的老路（仅用于对比排障）。

.PARAMETER StopAfterSeconds
  启动后 N 秒自动结束进程（脚本化验证用，例如 3）；默认 0 = 不自动结束。

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-dev.ps1

.EXAMPLE
  # 已构建过，只想重新签名并启动
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-dev.ps1 -NoBuild
#>
#Requires -Version 5.1
[CmdletBinding(PositionalBinding = $false)]
param(
    [string]$Root = '',
    [string]$Bin = 'CapLite',
    [switch]$Release,
    [string[]]$AppArgs = @(),
    [switch]$NoBuild,
    [switch]$NoStart,
    # 透传给 dev-sign.ps1：证书存储不可写时用工作区 PFX + 内存密钥签名
    # （签名能写进文件，但受信任存储装不进去，状态不会是 Valid —— 只用于受限环境排障）
    [switch]$PortableKeyFallback,
    # 用 Start-Process（UseShellExecute = true，经过 Shell/ShellExecuteEx）启动。
    # 默认**不用**它：ShellExecute 会走 Shell 的安全检查（"无法验证发布者"确认框就在这条路上），
    # 而直接 CreateProcess（UseShellExecute = $false）不经过 Shell，也就不会弹那个框。
    [switch]$UseShellExecute,
    [int]$StopAfterSeconds = 0
)

$ErrorActionPreference = 'Continue'

# ---------------------------------------------------------------------------
# 路径与环境
# ---------------------------------------------------------------------------
if ([string]::IsNullOrWhiteSpace($Root)) { $Root = Split-Path -Parent $PSScriptRoot }
$Root = (Resolve-Path -LiteralPath $Root).Path
$Workspace = Split-Path -Parent $Root
$TargetRoot = Join-Path $Workspace '_rust\target-cap'
$ProfileDir = 'debug'
if ($Release) { $ProfileDir = 'release' }

$envScript = Join-Path $Workspace '_rust\env.ps1'

function Get-CargoCommand {
    try { return (Get-Command cargo -ErrorAction Stop) } catch { return $null }
}

# 环境就绪判定（一次性）：
#   * FFMPEG_DIR 非空且目录存在 —— ffmpeg-sys-next 只认它（本机既无 pkg-config 也无 vcpkg）；
#   * PATH 里能找到 cargo。
# 两项都满足才跳过 env.ps1。**不能**只看"PATH 里有没有 cargo"：本机 ~\.cargo\bin 里有 rustup
# 垫片，旧逻辑会因此跳过 env.ps1 → FFMPEG_DIR 空 → cargo build 报 `error: FFmpeg not found.`。
# env.ps1 本身是幂等的（只赋 $env: 变量 + 覆盖式导入 vcvars 环境），这里仍用一次性判定，
# 免得每次运行都重导一遍 MSVC 环境、反复往 PATH 前面拼目录。
$cargoCmd = Get-CargoCommand
$ffmpegDirOk = (-not [string]::IsNullOrWhiteSpace($env:FFMPEG_DIR)) -and (Test-Path -LiteralPath $env:FFMPEG_DIR)

if ((-not $ffmpegDirOk) -or ($null -eq $cargoCmd)) {
    if (Test-Path -LiteralPath $envScript) {
        $why = @()
        if (-not $ffmpegDirOk) { $why += ("FFMPEG_DIR 未设置或目录不存在（当前：'{0}'）" -f $env:FFMPEG_DIR) }
        if ($null -eq $cargoCmd) { $why += 'PATH 里没有 cargo' }
        Write-Host ("[run-dev] 环境未就绪（{0}），导入构建环境：{1}" -f ($why -join '；'), $envScript)
        $prevEap = $ErrorActionPreference
        $envText = Get-Content -LiteralPath $envScript -Raw -Encoding UTF8
        Invoke-Expression $envText
        $ErrorActionPreference = $prevEap
        $cargoCmd = Get-CargoCommand
        if (-not [string]::IsNullOrWhiteSpace($env:FFMPEG_DIR)) {
            Write-Host ("[run-dev] 已引导环境（FFMPEG_DIR={0}）" -f $env:FFMPEG_DIR) -ForegroundColor Green
        } else {
            Write-Host '[run-dev] 已执行 env.ps1，但 FFMPEG_DIR 仍为空（随包 FFmpeg 开发包缺失？）' -ForegroundColor Yellow
        }
    } else {
        Write-Host "[run-dev] 环境未就绪，且找不到 $envScript" -ForegroundColor Yellow
    }
} else {
    Write-Host ("[run-dev] 环境已就绪（FFMPEG_DIR={0}，cargo={1}）" -f $env:FFMPEG_DIR, $cargoCmd.Source)
}
if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
    Write-Host '[run-dev] 提示：LIBCLANG_PATH 为空；若本次要重新跑 ffmpeg-sys-next 的 bindgen，请先执行 env.ps1。' -ForegroundColor Yellow
}
# 与本次开发保持一致：构建产物统一放 _rust\target-cap（env.ps1 里默认是 _rust\target）
$env:CARGO_TARGET_DIR = $TargetRoot
Write-Host "CARGO_TARGET_DIR = $env:CARGO_TARGET_DIR"

# ---------------------------------------------------------------------------
# 外部命令：分离捕获 stdout/stderr，返回真实退出码（同 regression.ps1）
# ---------------------------------------------------------------------------
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
    $psi.WorkingDirectory = $Root
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
        if (-not $proc.Start()) { return [pscustomobject]@{ Code = 9009; Out = ''; Err = "无法启动进程：$resolved" } }
        $outTask = $proc.StandardOutput.ReadToEndAsync()
        $errTask = $proc.StandardError.ReadToEndAsync()
        $proc.WaitForExit()
        return [pscustomobject]@{ Code = [int]$proc.ExitCode; Out = [string]$outTask.Result; Err = [string]$errTask.Result }
    } catch {
        return [pscustomobject]@{ Code = 9009; Out = ''; Err = "启动 $resolved 失败：$($_.Exception.Message)" }
    }
}

function Show-Text {
    param([string]$Text, [int]$Lines = 20, [string]$Prefix = '    | ')
    if ([string]::IsNullOrWhiteSpace($Text)) { return }
    $all = @($Text -split "`r?`n" | Where-Object { $_.Trim() -ne '' })
    $start = [Math]::Max(0, $all.Count - $Lines)
    for ($i = $start; $i -lt $all.Count; $i++) { Write-Host ($Prefix + $all[$i]) }
}

# ---------------------------------------------------------------------------
# 1) 构建
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host "== 1/3 构建 ($ProfileDir) ==" -ForegroundColor Cyan
$exePath = Join-Path (Join-Path $TargetRoot $ProfileDir) "$Bin.exe"

if ($NoBuild) {
    Write-Host "  -NoBuild：跳过 cargo build"
} else {
    $cargoArgs = @('build', '--bin', $Bin)
    if ($Release) { $cargoArgs += '--release' }
    Write-Host ("  cargo " + ($cargoArgs -join ' '))
    $r = Invoke-Tool 'cargo' $cargoArgs
    if ($r.Code -ne 0) {
        Write-Host "  cargo build 失败（exit=$($r.Code)）" -ForegroundColor Red
        Show-Text $r.Out 30
        Show-Text $r.Err 30
        if (("$($r.Out)`n$($r.Err)") -match 'FFmpeg not found') {
            Write-Host ''
            Write-Host "[run-dev] 构建失败：未找到 FFmpeg。请先在本窗口执行  . '$envScript'  再重试，或用 -NoBuild 跳过构建。" -ForegroundColor Yellow
        }
        exit 1
    }
    Show-Text $r.Err 8
    Write-Host "  cargo build OK" -ForegroundColor Green
}

if (-not (Test-Path -LiteralPath $exePath)) {
    Write-Host "  找不到构建产物：$exePath" -ForegroundColor Red
    Write-Host '  （-Release 时请确认是否真的构建过 release，或去掉 -NoBuild 重新构建）' -ForegroundColor Yellow
    exit 1
}

# ---------------------------------------------------------------------------
# 1b) 产物自检：exe 旁边要有 FFmpeg / OpenH264 运行期 DLL
#     缺 DLL 时直接启动只会得到加载器的一句"找不到 xxx.dll"，很难定位；
#     这里先点名缺哪个，并给出两种修法。
# ---------------------------------------------------------------------------
function Test-DllInDir {
    param([string]$Dir, [string]$Pattern)
    if ([string]::IsNullOrWhiteSpace($Dir)) { return $false }
    if (-not (Test-Path -LiteralPath $Dir)) { return $false }
    $hit = @(Get-ChildItem -LiteralPath $Dir -Filter $Pattern -File -ErrorAction SilentlyContinue)
    return ($hit.Count -gt 0)
}

$outDir = Split-Path -Parent $exePath
$dllPatterns = @('avcodec-*.dll', 'avformat-*.dll', 'avutil-*.dll', 'swresample-*.dll', 'swscale-*.dll', 'openh264*.dll')
$missingHere = @()
foreach ($pat in $dllPatterns) {
    if (-not (Test-DllInDir -Dir $outDir -Pattern $pat)) { $missingHere += $pat }
}

if ($missingHere.Count -eq 0) {
    Write-Host "  运行库自检 OK：avcodec/avformat/avutil/swresample/swscale/openh264 DLL 均在 $outDir" -ForegroundColor Green
} else {
    # exe 旁边没有，但 PATH 里能解析到也照样能启动（env.ps1 会把 FFMPEG_DIR\bin 放进 PATH）
    $resolvedInPath = @{}
    $unresolved = @()
    foreach ($pat in $missingHere) {
        $found = $null
        foreach ($entry in ($env:Path -split ';')) {
            $d = $entry.Trim().Trim('"')
            if (Test-DllInDir -Dir $d -Pattern $pat) { $found = $d; break }
        }
        if ($found) { $resolvedInPath[$pat] = $found } else { $unresolved += $pat }
    }
    Write-Host ("  以下 DLL 不在 exe 旁边：{0}" -f ($missingHere -join ', ')) -ForegroundColor Yellow
    foreach ($pat in $resolvedInPath.Keys) {
        Write-Host ("    - {0}：PATH 里可解析（{1}）" -f $pat, $resolvedInPath[$pat])
    }
    if ($unresolved.Count -gt 0) {
        Write-Host ("  运行库自检失败，找不到这些 DLL：{0}" -f ($unresolved -join ', ')) -ForegroundColor Red
        Write-Host '  修法一：在**已引导环境**（FFMPEG_DIR 已设置）的 shell 里重新构建（本脚本去掉 -NoBuild 直接跑即可；cap 的 build.rs 会把 DLL 复制到 exe 同级）' -ForegroundColor Yellow
        Write-Host ("  修法二：手动从 {0} 把同名 DLL 拷到 {1}" -f (Join-Path $Root 'ffmpeg-n9.0-latest-win64-lgpl-shared-9.0\bin'), $outDir) -ForegroundColor Yellow
        Write-Host ("          openh264*.dll 的来源是 {0}" -f (Join-Path $Root 'native\openh264.dll')) -ForegroundColor Yellow
        exit 1
    }
}

# ---------------------------------------------------------------------------
# 2) 签名（清 MotW + 自签名 + 回读校验）
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '== 2/3 开发签名 ==' -ForegroundColor Cyan
$signScript = Join-Path $PSScriptRoot 'dev-sign.ps1'
if (-not (Test-Path -LiteralPath $signScript)) {
    Write-Host "  找不到签名脚本：$signScript" -ForegroundColor Red
    exit 1
}
& $signScript -Path @($exePath) -PortableKeyFallback:$PortableKeyFallback
$signExit = 0
if ($null -ne $LASTEXITCODE) { $signExit = [int]$LASTEXITCODE }
if ($signExit -ne 0) {
    Write-Host ''
    Write-Host "  [WARN] 签名没有全部通过（dev-sign.ps1 exit=$signExit）：启动时仍可能看到发行者/ SmartScreen 提示。" -ForegroundColor Yellow
    Write-Host '         若提示来自"证书存储装不进去"，请在普通 PowerShell（非沙箱）里重跑本脚本。' -ForegroundColor Yellow
} else {
    Write-Host '  签名 OK（Status=Valid，MotW 已清）' -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# 3) 启动
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host '== 3/3 启动 ==' -ForegroundColor Cyan
if ($NoStart) {
    Write-Host '  -NoStart：不启动'
    exit $signExit
}

# 默认走 ProcessStartInfo(UseShellExecute = $false)：直接 CreateProcess，
# 不经过 Shell 的附件安全检查 —— 也就是不会出现「无法验证发布者。你确定要运行此软件吗？」，
# 而且不会因为模态框卡住自动化。加 -UseShellExecute 才回到 Start-Process 的老路（用于对比）。
$workDir = Split-Path -Parent $exePath
try {
    if ($UseShellExecute) {
        $startArgs = @{ FilePath = $exePath; WorkingDirectory = $workDir }
        if ($AppArgs.Count -gt 0) { $startArgs.ArgumentList = $AppArgs }
        Write-Host '  启动方式：Start-Process（UseShellExecute = $true，会经过 Shell 的安全检查）' -ForegroundColor Yellow
        $proc = Start-Process @startArgs -PassThru
    } else {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $exePath
        $psi.WorkingDirectory = $workDir
        $psi.UseShellExecute = $false
        if ($AppArgs.Count -gt 0) {
            $quoted = @()
            foreach ($a in $AppArgs) {
                $s = [string]$a
                if ($s -match '[\s"]') { $s = '"' + ($s -replace '"', '\"') + '"' }
                $quoted += $s
            }
            $psi.Arguments = ($quoted -join ' ')
        }
        Write-Host '  启动方式：ProcessStartInfo(UseShellExecute = $false) —— 直接 CreateProcess，不经过 Shell'
        $proc = [System.Diagnostics.Process]::Start($psi)
    }
    if ($null -eq $proc) { throw '进程对象为空（启动未成功）' }
    Write-Host "  已启动：$exePath  (PID $($proc.Id))" -ForegroundColor Green
} catch {
    Write-Host "  启动失败：$($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

if ($StopAfterSeconds -gt 0) {
    Start-Sleep -Seconds $StopAfterSeconds
    try { Stop-Process -Id $proc.Id -Force -ErrorAction Stop; Write-Host "  已结束进程 PID $($proc.Id)（-StopAfterSeconds $StopAfterSeconds）" }
    catch { Write-Host "  结束进程失败：$($_.Exception.Message)" -ForegroundColor Yellow }
}

exit $signExit
