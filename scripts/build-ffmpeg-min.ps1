<#
.SYNOPSIS
    构建"最小配置" FFmpeg LGPLv3 共享版（5 个 DLL，供 CapLite 使用）。

.DESCRIPTION
    本文件位于 `CapLite\scripts\build-ffmpeg-min.ps1`（2026-10-13 从 `_rust\build-ffmpeg-min.ps1`
    正式移入脚本目录）。`CapLite\scripts\**` 解冻后这是**唯一一份**，源码获取方式即本脚本。

    产出：`<workspace>\CapLite\ffmpeg-min\`，内含
        bin\{avcodec-63,avformat-63,avutil-61,swresample-7,swscale-10}.dll
        lib\*.lib（MSVC 导入库）+ lib\*.dll.a（MinGW 导入库）
        include\（libav* 头文件）
    也就是说 ffmpeg-min 本身就是一个**完整可用的 FFMPEG_DIR**（头文件 + 导入库 + 运行库齐全）。

    本脚本可在**任意联网 HTTP 环境下重跑**（本机 HTTPS 不通，见下"网络"）。

    ── 版本与许可 ────────────────────────────────────────────────────────────
    FFmpeg 版本 : 9.0.2   （libavutil 61.1.102 / libavcodec 63.1.102 / libavformat 63.1.102）
    源码来源    : http://mirrors.tuna.tsinghua.edu.cn/debian/pool/main/f/ffmpeg/ffmpeg_9.0.2.orig.tar.xz
                  （Debian pool 里的 .orig.tar.xz 就是上游 pristine tarball）
    SHA256      : 8C3850283EB25FA026482078A04051E0BE17347B09EF81A0849BEC15A96E002E
    许可        : --enable-shared --enable-version3，**没有** --enable-gpl / --enable-nonfree
                  → configure 自报 "License: LGPL version 3 or later"
                  动态链接（用户可自行替换 DLL）满足 LGPL 要求；源码获取方式见上"源码来源"。

    ── configure 参数（与人工构建逐字一致，勿随手改）────────────────────────
    --prefix=<短路径>\CapLite\ffmpeg-min
    --arch=x86_64 --target-os=mingw32
    --enable-shared --disable-static --enable-version3
    --disable-everything --disable-programs --disable-doc --disable-debug --disable-network
    --disable-avdevice --disable-avfilter --disable-autodetect
    --enable-avcodec --enable-avformat --enable-avutil --enable-swresample --enable-swscale
    --enable-encoder=aac --enable-decoder=h264 --enable-decoder=aac --enable-parser=h264
    --enable-muxer=mp4 --enable-muxer=mov --enable-demuxer=mov --enable-protocol=file
    --enable-small --extra-version=capmin --extra-ldflags=-static-libgcc
    注意 1：**没有** --disable-postproc —— FFmpeg 9.0 已移除 postproc，带上会 "Unknown option" 报错。
    注意 2（2026-10-14 本轮改动）：新增 **--enable-decoder=aac** —— 剪辑页音频波形必须自己解码
      AAC 音轨（录制侧只**编码** AAC，从来不解码）。AAC 解码器自带 `aac_parser` 依赖，
      configure 会自动把它打开，所以**不需要**显式写 `--enable-parser=aac`
      （写了也合法，但会让"改了什么"变得含糊）；同样自动带进来的还有 `aac_ac3_parser` 与
      比特流过滤器 `aac_adtstoasc`。实测 avcodec-63.dll 2 087 424 → 2 225 664 B（+138 240 B，
      5 个 DLL 合计 4 407 808 → 4 546 048 B）；configure 自报 `Enabled decoders: aac h264`、
      `License: LGPL version 3 or later`（没有 --enable-gpl / --enable-nonfree）。

    ── 必要的外部工具（本机既有，脚本只做探测）──────────────────────────────
    mingw-w64 gcc/ar/nm/strip/dlltool/windres : C:\mingw64\bin        （GCC 14.2.0）
    GNU make                                 : C:\mingw64\bin\mingw32-make.exe（4.4.1）
    nasm                                     : C:\nasm\nasm.exe       （3.02）
    POSIX shell                              : busybox-w32（见下）

    ── 网络（重要）──────────────────────────────────────────────────────────
    本机 HTTPS 不可用（schannel 报错），**只有 HTTP(80)**。可用的 HTTP 通道：
      * 清华 TUNA        http://mirrors.tuna.tsinghua.edu.cn/...
      * SourceForge 直链 http://prdownloads.sourceforge.net/<proj>/<file>
                         http://downloads.sourceforge.net/project/<proj>/<path>/<file>
                        （项目页面会 301 到 https，但上面两个**直链是纯 HTTP 200**）
    已实测不可用：ffmpeg.org / git.ffmpeg.org（http→301 https）、
      mirrors.{tuna,aliyun,ustc} 的 gentoo distfiles（无 ffmpeg）、
      sources.buildroot.net/ffmpeg/（404）、各种 gh-proxy（全跳 https）。

    ── 为什么必须用 busybox-w32 而不能用 Git 自带的 bash ────────────────────
    DSH 沙箱禁止 `Global\` 内核对象命名空间（`CreateFileMapping Global\...` 报 Win32 error 5），
    而 MSYS2/Cygwin 运行时的共享内存正好用它 → Git for Windows 里的 bash/sh/dash/sed/grep/awk/ls
    **全部**起不来（`couldn't create signal pipe, Win32 error 5`）。
    busybox-w32 是**原生 Win32** 二进制（只依赖 KERNEL32/USER32/ADVAPI32/msvcrt/WS2_32），
    内置 ash + 156 个 applet，完全够 configure 用。

    ── 三个"必须"的 make 覆盖（踩坑记录）────────────────────────────────────
    1) AR_CMD / NM_CMD：FFmpeg 的 SLIB_CREATE_DEF_CMD 用的是 $(AR_CMD)/$(NM_CMD)
       （ffbuild/config.mak 里默认写死 ar / "nm -g"）。busybox 的 ash 启用了
       standalone applet 派发，**不看 PATH** 就会命中 busybox 自带的 ar applet，
       报 "sh: unknown option -- s" / "Could not create temporary library."
       所以必须用绝对路径覆盖。
    2) LN_S="cp -f"：Windows 建不了符号链接，默认的 `ln -s -f` 会报
       "ln: avutil.dll: Function not implemented" 卡在 libavutil/avutil.dll。
    3) --extra-ldflags=-static-libgcc：否则 swscale-10.dll 会依赖 libgcc_s_seh-1.dll，
       而发布包里没有这个文件。

    ── 路径里的空格（致命）──────────────────────────────────────────────────
    工作区路径含空格（`C:\Users\Limit li\...`）。FFmpeg 的 configure 经不起空格：
    会报 `configure: eval: line 1: li/Documents/.../test.c: not found` 并最终
    `gcc is unable to create an executable file.` / `C compiler test failed.`
    → 必须用 8.3 短路径，并且**连 TMP/TEMP/TMPDIR 一起**指向短路径下的目录。

.EXAMPLE
    pwsh -NoProfile -File scripts\build-ffmpeg-min.ps1
    pwsh -NoProfile -File scripts\build-ffmpeg-min.ps1 -Jobs 12 -Force
#>
[CmdletBinding()]
param(
    # 工作区根目录（默认为脚本所在目录的**上两级**：脚本在 CapLite\scripts\ 下）
    [string]$Workspace = '',
    # 并行编译任务数
    [int]$Jobs = 8,
    # 源码目录名（默认按版本推导）
    [string]$SourceDirName = '',
    # 已解包的源码目录（给了就跳过下载/解包）
    [string]$SourceDir = '',
    # 忽略已存在的结果，强制重下/重编
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$FFmpegVersion = '9.0.2'
$TarballName   = "ffmpeg_${FFmpegVersion}.orig.tar.xz"
$TarballUrl    = "http://mirrors.tuna.tsinghua.edu.cn/debian/pool/main/f/ffmpeg/$TarballName"
$TarballSha256 = '8C3850283EB25FA026482078A04051E0BE17347B09EF81A0849BEC15A96E002E'
$BusyboxUrl    = 'http://prdownloads.sourceforge.net/busybox-for-windows/BusyBox-1.33_Win32.zip'
$BusyboxSha256 = '65A9CB662560C76A0EB3622340F2757BF4B9F088867D17A606C389D8522553B8'
# 不要用 Git for Windows 自带的 msys 目录（沙箱下全废）
$MingwBin = 'C:\mingw64\bin'
$NasmDir  = 'C:\nasm'

function Say ([string]$m, [string]$c = 'Gray') { Write-Host "[ffbuild] $m" -ForegroundColor $c }
function Die ([string]$m) { Write-Host "[ffbuild] 失败：$m" -ForegroundColor Red; exit 1 }

# 脚本位于 <工作区>\CapLite\scripts\，所以默认工作区根 = 上两级目录。
# （移入 scripts\ 前的旧位置是 <工作区>\_rust\，那时只需上一级；这是一处必须同步的路径修正。）
if ([string]::IsNullOrWhiteSpace($Workspace)) { $Workspace = Split-Path -Parent (Split-Path -Parent $PSScriptRoot) }
$Workspace = (Resolve-Path -LiteralPath $Workspace).Path
$RustRoot  = Join-Path $Workspace '_rust'
$ToolsDir  = Join-Path $RustRoot 'tools'
$DlDir     = Join-Path $ToolsDir 'dl'
$SrcDirRoot= Join-Path $ToolsDir 'src'
$BbDir     = Join-Path $ToolsDir 'bbbin'
$PrefixLong= Join-Path $Workspace 'CapLite\ffmpeg-min'

if ([string]::IsNullOrWhiteSpace($SourceDirName)) { $SourceDirName = "ffmpeg-$FFmpegVersion" }
if ([string]::IsNullOrWhiteSpace($SourceDir))     { $SourceDir = Join-Path $SrcDirRoot $SourceDirName }

# ---------------------------------------------------------------------------
# 0. 8.3 短路径（FFmpeg configure 经不起路径里的空格）
# ---------------------------------------------------------------------------
function Get-ShortPath ([string]$p) {
    $s = & cmd.exe /c "for %I in (`"$p`") do @echo %~sI" 2>$null
    if ([string]::IsNullOrWhiteSpace($s)) { return $p }
    return ($s | Select-Object -First 1).Trim()
}

# ---------------------------------------------------------------------------
# 1. 探测 / 准备工具链
# ---------------------------------------------------------------------------
Say '--- 1. 工具链 ---'
foreach ($t in @("$MingwBin\gcc.exe", "$MingwBin\mingw32-make.exe", "$MingwBin\ar.exe", "$MingwBin\nm.exe", "$MingwBin\dlltool.exe", "$NasmDir\nasm.exe")) {
    if (-not (Test-Path -LiteralPath $t)) { Die "缺少 $t（需要 mingw-w64 + nasm；本机既有）" }
}
$gccVer = (& "$MingwBin\gcc.exe" -dumpfullversion 2>&1 | Select-Object -First 1)
$mkVer  = (& "$MingwBin\mingw32-make.exe" --version 2>&1 | Select-Object -First 1)
$nasmV  = (& "$NasmDir\nasm.exe" -v 2>&1 | Select-Object -First 1)
Say ("gcc  = {0}" -f $gccVer)
Say ("make = {0}" -f $mkVer)
Say ("nasm = {0}" -f $nasmV)
Say '（bash 不可用：DSH 沙箱封 Global\ 命名空间 → MSYS2/Cygwin 全线起不来，故用 busybox-w32）'

New-Item -ItemType Directory -Force -Path $DlDir, $SrcDirRoot | Out-Null

# busybox-w32：原生 POSIX shell（ash）+ 156 个 applet
$BbExe = Join-Path $ToolsDir 'busybox\BusyBox-1.33_Win32\busybox.exe'
if ($Force -or -not (Test-Path -LiteralPath $BbExe)) {
    $zip = Join-Path $DlDir 'BusyBox-1.33_Win32.zip'
    Say "下载 busybox-w32 -> $zip"
    & curl.exe -L --fail --max-time 600 -o $zip $BusyboxUrl
    if ($LASTEXITCODE -ne 0) { Die "busybox 下载失败（curl exit $LASTEXITCODE）" }
    $h = (Get-FileHash $zip -Algorithm SHA256).Hash
    if ($h -ne $BusyboxSha256) { Die "busybox SHA256 不符`n  期望 $BusyboxSha256`n  实际 $h" }
    $bbRoot = Join-Path $ToolsDir 'busybox'
    New-Item -ItemType Directory -Force -Path $bbRoot | Out-Null
    # 注意：不要用 Git 自带的 tar.exe（msys 运行时同样起不来），用系统 bsdtar
    & "$env:SystemRoot\System32\tar.exe" -xf $zip -C $bbRoot
    if ($LASTEXITCODE -ne 0) { Die "busybox 解包失败" }
}
if (-not (Test-Path -LiteralPath $BbExe)) { Die "找不到 busybox.exe：$BbExe" }

# 安装 applet 硬链接（sh.exe / sed.exe / awk.exe / grep.exe / ...）到 _rust\tools\bbbin
New-Item -ItemType Directory -Force -Path $BbDir | Out-Null
if ($Force -or -not (Test-Path -LiteralPath (Join-Path $BbDir 'sh.exe'))) {
    Say "安装 busybox applet 到 $BbDir"
    & $BbExe --install $BbDir | Out-Null
}
if (-not (Test-Path -LiteralPath (Join-Path $BbDir 'sh.exe'))) { Die "applet 安装失败（缺 $BbDir\sh.exe）" }
# FFmpeg 的 Makefile 直接调 `make`
Copy-Item "$MingwBin\mingw32-make.exe" (Join-Path $BbDir 'make.exe') -Force
$appletCount = (Get-ChildItem $BbDir -File | Measure-Object).Count
Say "applet 就绪：$appletCount 个（$BbDir）"

# ---------------------------------------------------------------------------
# 2. 源码
# ---------------------------------------------------------------------------
Say '--- 2. FFmpeg 源码 ---'
if (-not (Test-Path -LiteralPath (Join-Path $SourceDir 'configure'))) {
    $tarball = Join-Path $DlDir $TarballName
    if ($Force -or -not (Test-Path -LiteralPath $tarball)) {
        Say "下载 $TarballName -> $tarball"
        & curl.exe -L --fail --max-time 1800 -o $tarball $TarballUrl
        if ($LASTEXITCODE -ne 0) { Die "源码下载失败（curl exit $LASTEXITCODE）" }
    }
    $h = (Get-FileHash $tarball -Algorithm SHA256).Hash
    if ($h -ne $TarballSha256) { Die "源码 SHA256 不符`n  期望 $TarballSha256`n  实际 $h" }
    Say "SHA256 校验通过"
    New-Item -ItemType Directory -Force -Path $SrcDirRoot | Out-Null
    Say "解包（用系统 bsdtar，不用 Git 的 tar）"
    & "$env:SystemRoot\System32\tar.exe" -xf $tarball -C $SrcDirRoot
    if ($LASTEXITCODE -ne 0) { Die "源码解包失败" }
}
if (-not (Test-Path -LiteralPath (Join-Path $SourceDir 'configure'))) { Die "源码目录不完整：$SourceDir" }
Say "源码：$SourceDir"

# ---------------------------------------------------------------------------
# 3. 短路径 + 环境
# ---------------------------------------------------------------------------
Say '--- 3. 短路径与环境 ---'
$shortWs   = Get-ShortPath $Workspace
$shortSrc  = Get-ShortPath $SourceDir
$shortTmp  = Get-ShortPath (Join-Path $RustRoot 'tmp')
$shortBb   = Get-ShortPath $BbDir
New-Item -ItemType Directory -Force -Path $shortTmp | Out-Null
$shortPrefix = "$shortWs\CapLite\ffmpeg-min"
Say "workspace 短路径 : $shortWs"
Say "源码     短路径 : $shortSrc"
Say "TMP      短路径 : $shortTmp"
if ($shortSrc -match ' ') { Die "短路径仍含空格，configure 会失败：$shortSrc" }

# PATH 顺序：MinGW binutils 在前（但注意 busybox ash 的 standalone 派发会绕过 PATH，
# 所以 ar/nm 仍必须用 AR_CMD/NM_CMD 覆盖）；busybox 提供 sh/sed/awk/...；System32 最后。
$env:Path = "$MingwBin;$shortBb;$NasmDir;$env:SystemRoot\System32;$env:SystemRoot;$env:SystemRoot\System32\Wbem"
$env:TMP = $shortTmp; $env:TEMP = $shortTmp; $env:TMPDIR = $shortTmp

$bb  = Join-Path $shortBb 'busybox.exe'
$mk  = Join-Path $shortBb 'make.exe'
$AR  = ($MingwBin + '\ar.exe').Replace('\', '/')
$NM  = ($MingwBin + '\nm.exe').Replace('\', '/')

# ---------------------------------------------------------------------------
# 4. configure
# ---------------------------------------------------------------------------
Say '--- 4. configure（约 2 分钟）---'
$cfgArgs = @(
    'configure',
    "--prefix=$shortPrefix",
    '--arch=x86_64', '--target-os=mingw32',
    '--enable-shared', '--disable-static', '--enable-version3',
    '--disable-everything', '--disable-programs', '--disable-doc', '--disable-debug', '--disable-network',
    '--disable-avdevice', '--disable-avfilter', '--disable-autodetect',
    '--enable-avcodec', '--enable-avformat', '--enable-avutil', '--enable-swresample', '--enable-swscale',
    '--enable-encoder=aac', '--enable-decoder=h264', '--enable-decoder=aac', '--enable-parser=h264',
    '--enable-muxer=mp4', '--enable-muxer=mov', '--enable-demuxer=mov', '--enable-protocol=file',
    '--enable-small', '--extra-version=capmin', '--extra-ldflags=-static-libgcc'
)
$cfgLog = Join-Path $shortTmp 'build-ffmpeg-min.configure.log'
Push-Location $shortSrc
try {
    & $bb sh @cfgArgs *> $cfgLog
    if ($LASTEXITCODE -ne 0) { Die "configure 失败（exit $LASTEXITCODE），日志：$cfgLog" }
} finally { Pop-Location }
$lic = (Select-String -Path $cfgLog -Pattern '^License:' | Select-Object -First 1).Line
if (-not $lic) { Die "configure 未输出 License 行，日志：$cfgLog" }
Say $lic
if ($lic -notmatch 'LGPL') { Die "许可不是 LGPL：$lic（不得出现 --enable-gpl / --enable-nonfree）" }

# ---------------------------------------------------------------------------
# 5. make && make install
# ---------------------------------------------------------------------------
Say "--- 5. make -j$Jobs（全量约 30 秒）---"
$mkOverrides = @("AR_CMD=$AR", "NM_CMD=$NM -g", 'LN_S=cp -f')
$makeLog = Join-Path $shortTmp 'build-ffmpeg-min.make.log'
Push-Location $shortSrc
try {
    & $mk "-j$Jobs" @mkOverrides *> $makeLog
    if ($LASTEXITCODE -ne 0) { Die "make 失败（exit $LASTEXITCODE），日志：$makeLog" }
    Say 'make install'
    & $mk install @mkOverrides *> (Join-Path $shortTmp 'build-ffmpeg-min.install.log')
    if ($LASTEXITCODE -ne 0) { Die "make install 失败（日志：$shortTmp\build-ffmpeg-min.install.log）" }
} finally { Pop-Location }

# ---------------------------------------------------------------------------
# 6. 结果与体积对比
# ---------------------------------------------------------------------------
Say '--- 6. 结果 ---'
$binDir = Join-Path $PrefixLong 'bin'
$names = @('avcodec-63.dll', 'avformat-63.dll', 'avutil-61.dll', 'swresample-7.dll', 'swscale-10.dll')
$oldDir = Join-Path $Workspace 'CapLite\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0\bin'

$rows = @(); $sumNew = 0; $sumOld = 0
foreach ($n in $names) {
    $new = Get-Item -LiteralPath (Join-Path $binDir $n) -ErrorAction SilentlyContinue
    if (-not $new) { Die "缺少产物 $binDir\$n" }
    $old = Get-Item -LiteralPath (Join-Path $oldDir $n) -ErrorAction SilentlyContinue
    $sumNew += $new.Length
    if ($old) { $sumOld += $old.Length }
    $rows += [pscustomobject]@{
        DLL = $n
        '旧(B)' = if ($old) { $old.Length } else { '-' }
        '新(B)' = $new.Length
        '压缩'  = if ($old) { '{0:N1}%' -f (100.0 * (1 - $new.Length / $old.Length)) } else { '-' }
        SHA256_16 = (Get-FileHash $new.FullName -Algorithm SHA256).Hash.Substring(0, 16)
    }
}
$rows | Format-Table -AutoSize
Say ("新合计 {0:N0} B ({1:N2} MB)" -f $sumNew, ($sumNew / 1MB)) 'Green'
if ($sumOld -gt 0) {
    Say ("旧合计 {0:N0} B ({1:N2} MB)  →  省 {2:N2} MB（{3:N1}%）" -f `
        $sumOld, ($sumOld / 1MB), (($sumOld - $sumNew) / 1MB), (100.0 * (1 - $sumNew / $sumOld))) 'Green'
}

# 依赖自检：只允许 OS 自带 DLL + 同套 av*/sw*（不应出现 libgcc_s_seh-1.dll / libwinpthread-1.dll）
$objdump = Join-Path $MingwBin 'objdump.exe'
$sys = '^(KERNEL32|USER32|ADVAPI32|SHELL32|ole32|oleaut32|GDI32|WS2_32|bcrypt|msvcrt|VCRUNTIME|api-ms-win-)'
foreach ($n in $names) {
    $deps = & $objdump -p (Join-Path $binDir $n) 2>&1 |
            Select-String 'DLL Name' | ForEach-Object { ($_.Line -split 'DLL Name: ')[1].Trim() }
    $bad = $deps | Where-Object { $_ -notmatch $sys -and $_ -notmatch '^(av|sw)[a-z]+-\d+\.dll$' }
    if ($bad) { Say ("  依赖告警 {0}: {1}" -f $n, ($bad -join ', ')) 'Yellow' }
}
Say '依赖自检完成（无输出＝只有 OS DLL + 同套 av*/sw*）'
Say ("产物目录：{0}" -f $PrefixLong) 'Green'
Say '提示：本目录（include + lib + bin）本身就是一个完整可用的 FFMPEG_DIR，可直接赋给 $env:FFMPEG_DIR。'
