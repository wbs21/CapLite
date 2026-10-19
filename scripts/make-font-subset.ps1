# 生成 **中文子集字体** `assets/font-subset.ttf`（启动优化第 3 项）。
#
# 为什么要子集：界面语言包 / 源码里的中文字面量只用到几百个汉字，而系统完整中文字体
# （`msyh.ttc` 19 MB / `Deng.ttf` 16 MB / `simsun.ttc` 18 MB）读进来 + 建字形图集要几十到
# 几百毫秒，全部压在"首帧之前"。把**真正用到的字形**抽成几十~几百 KB 的内嵌字体先装上，
# 完整系统字体改到首帧之后追加当兜底（生僻字最终仍能显示）。
#
# 用法（不需要参数）：
#     powershell -NoProfile -ExecutionPolicy Bypass -File scripts\make-font-subset.ps1
#
# 依赖：本机 Python + `fontTools`（HTTPS 不可用时用 HTTP 镜像装）：
#     python -m pip install fonttools brotli -i http://mirrors.aliyun.com/pypi/simple/ --trusted-host mirrors.aliyun.com
# 缺依赖时脚本**直接失败并打印这条命令**，绝不悄悄产出一个不完整的字体。
#
# 产物：`assets/font-subset.ttf`（**要随源码提交**：`src/main.rs` 用 `include_bytes!` 内嵌它）。
# 更新时机：改了 `locales/*.json` 的文案、或新增了界面里的中文字面量之后，**重跑本脚本**。

[CmdletBinding()]
param(
    # 源字体（ttc/ttf）。默认按与运行期相同的优先级挑第一个存在的。
    [string]$SourceFont = '',
    # 源字体在 ttc 里的字面号（ttf 用 0）
    [int]$FaceIndex = 0,
    # 额外的 Python 搜索路径（把 fontTools 解开到某个目录时用，见下面注释）
    [string]$PythonPath = '',
    # 只生成字符清单、不生成字体（用于排查"哪个字没被包含进来"）
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot          # 仓库根（CapLite）
$OutFile = Join-Path $Root 'assets\font-subset.ttf'
$PyScript = Join-Path $PSScriptRoot 'make-font-subset.py'

# 与 src/main.rs 的 SYSTEM_CJK_CANDIDATES 保持一致的顺序
$Candidates = @(
    'C:\Windows\Fonts\msyh.ttc',
    'C:\Windows\Fonts\msyh.ttf',
    'C:\Windows\Fonts\Deng.ttf',
    'C:\Windows\Fonts\simhei.ttf',
    'C:\Windows\Fonts\simsun.ttc'
)

if (-not $SourceFont) {
    $SourceFont = $Candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $SourceFont -or -not (Test-Path $SourceFont)) {
    throw "找不到任何系统中文字体（试过：$($Candidates -join ', ')）。用 -SourceFont <路径> 显式指定。"
}

# 本机 Python（DSH 运行时自带；PATH 里没有 python 时用它）
$Python = 'C:\Users\Limit li\.dsh\dsh-runtimes\dsh-primary-runtime\dependencies\python\python.exe'
if (-not (Test-Path $Python)) {
    $cmd = Get-Command python -ErrorAction SilentlyContinue
    if ($cmd) { $Python = $cmd.Source } else { throw "找不到 Python：$Python" }
}

Write-Host "源字体    : $SourceFont (face #$FaceIndex)"
Write-Host "输出      : $OutFile"
Write-Host "Python    : $Python"

# fontTools 搜索路径：优先用 -PythonPath；否则沿用环境里的 PYTHONPATH。
# 本机 HTTPS 不可用时可以用 `_rust\tmp\fetch-fonttools.py` 把 wheel 直接解开到目录里，
# 然后把那个目录传给 -PythonPath（或设成环境变量 PYTHONPATH）。
if ($PythonPath) {
    if (Test-Path $PythonPath) {
        $env:PYTHONPATH = if ($env:PYTHONPATH) { "$PythonPath;$env:PYTHONPATH" } else { $PythonPath }
    } else {
        Write-Warning "PythonPath 不存在，忽略: $PythonPath"
    }
}
if ($env:PYTHONPATH) { Write-Host "PYTHONPATH: $env:PYTHONPATH" }

# 先确认 fontTools 可用：缺依赖要**明确失败**，不能产出一个不完整的字体
& $Python -c "import fontTools, sys; print('fontTools', fontTools.version)"
if ($LASTEXITCODE -ne 0) {
    throw @"
缺少 fontTools。请任选一种方式装上后再重跑：
  a) pip（HTTPS 可用或走 HTTP 镜像）：
     $Python -m pip install fonttools brotli -i http://mirrors.aliyun.com/pypi/simple/ --trusted-host mirrors.aliyun.com
  b) HTTPS 不可用、pip 卡住时（本机实测可用）：把 wheel 直接解开到本地目录
     $Python _rust\tmp\fetch-fonttools.py --target _rust\tmp\pylibs --packages fonttools
     然后加参数：-PythonPath _rust\tmp\pylibs
"@
}

$args = @($PyScript, '--source', $SourceFont, '--face-index', "$FaceIndex",
          '--root', $Root, '--out', $OutFile)
if ($DryRun) { $args += '--dry-run' }

& $Python @args
if ($LASTEXITCODE -ne 0) { throw "生成子集字体失败（Python 退出码 $LASTEXITCODE）" }

if (-not $DryRun) {
    $info = Get-Item $OutFile
    Write-Host ("产物体积  : {0:N0} 字节（{1:N1} KiB）" -f $info.Length, ($info.Length / 1KB))
}
