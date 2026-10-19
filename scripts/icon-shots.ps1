# icon-shots.ps1 -- 采集"运行时窗口图标"的客观证据（标题栏左上角 + 任务栏按钮）。
#
# 为什么需要这个脚本：`assets/caplite.ico` 进的是 **PE 资源节**（文件图标，资源管理器/文件属性
# 里那个），而**窗口图标**（标题栏 + 任务栏）是进程运行时用 `WM_SETICON` 设到 HWND 上的。两者
# 没有任何关系，**只有像素能证明**运行时到底画的是哪个。见 docs\DESIGN.md §11.26。
#
# 做法（全部只用 user32/gdi32 + .NET，不装任何东西）：
#   1. 记下 `Shell_TrayWnd`（任务栏）的矩形，先抓一张"启动前"基线；
#   2. `ProcessStartInfo` + `UseShellExecute=$false` 启动 exe（`--config-dir` 指向临时空目录）；
#   3. 按 PID `EnumWindows` 找**可见 + 带非客户区**的顶层窗口。
#      **不要用 `Process.MainWindowHandle`**：winit 会先建一个隐藏的消息窗口
#      （class `Winit Thread Event Target`，无标题栏），拿到的经常是它；
#   4. 摆到 (40,40)、`PrintWindow(..., PW_RENDERFULLCONTENT=2)` 抓整窗（**不需要抢前台**：
#      本会话是 Low 完整性，`SetForegroundWindow` 有时被拒，`CopyFromScreen` 就会抓到
#      盖在上面的别的窗口）。标题栏里的图标用"最右边那段 >= 6 px 宽的蓝色列区间"定位
#      ——最左那段可能属于紧挨着我们的**邻居窗口**的图标；
#   5. 任务栏用"启动前 / 启动后"的像素差定位新出现的按钮（差集只可能是我们的按钮）；
#   6. 输出 16×16 / 24×24 原图 + 10 倍最近邻放大图，供人眼确认"蓝色方块 + 白色 Cap"。
#
# 用法：
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\icon-shots.ps1 `
#       -Exe dist\CapLite-1.0.0\CapLite.exe -OutDir <目录>
#
# ✱ 只写**本脚本自己的**输出目录；不碰仓库里的其它文件。
[CmdletBinding(PositionalBinding = $false)]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir,
    # 等主窗口出现的上限（ms）
    [int]$WaitMs = 20000
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class CapIconShot {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr wp, IntPtr lp);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
  public static extern IntPtr FindWindowW(string cls, string win);
  public delegate bool EnumProc(IntPtr h, IntPtr lp);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lp);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int n);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int n);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public static string Txt(IntPtr h){ var sb=new StringBuilder(512); GetWindowTextW(h,sb,512); return sb.ToString(); }
  public static string Cls(IntPtr h){ var sb=new StringBuilder(256); GetClassNameW(h,sb,256); return sb.ToString(); }
  public static uint Pid(IntPtr h){ uint p; GetWindowThreadProcessId(h, out p); return p; }
  public static System.Collections.Generic.List<IntPtr> TopLevel(uint want) {
    var list = new System.Collections.Generic.List<IntPtr>();
    EnumWindows(delegate(IntPtr h, IntPtr lp) { if (Pid(h) == want) list.Add(h); return true; }, IntPtr.Zero);
    return list;
  }
}
"@

# 失败时把**出错位置**打出来（否则 PowerShell 只给一句"不能在空值上调用方法"，无法定位）
trap {
    Write-Host "[icon-shots] TRAP: $_"
    Write-Host $_.InvocationInfo.PositionMessage
    exit 1
}

if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Force -Path $OutDir | Out-Null }
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

function Save-Png {
    param([System.Drawing.Bitmap]$Bitmap, [string]$Path)
    $Bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
}

function Scale-Nearest {
    param([System.Drawing.Bitmap]$Bitmap, [int]$Factor)
    $big = New-Object System.Drawing.Bitmap ($Bitmap.Width * $Factor), ($Bitmap.Height * $Factor)
    $g = [System.Drawing.Graphics]::FromImage($big)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::NearestNeighbor
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::Half
    $g.DrawImage($Bitmap, 0, 0, $big.Width, $big.Height)
    $g.Dispose()
    return $big
}

# 蓝色列区间（brand 蓝 #2563EB..#3B82F6：B 明显压过 R/G）
#
# 注意：这里**不用** `$runs += ,@(...)` 往数组里塞"嵌套数组"。实测在 Windows PowerShell 5.1 下
# 这种写法连 `$x - 1` 都会抛 `[System.Object[]] does not contain a method named 'op_Subtraction'`
# （`$start` 被静默变成了 object[]）。改用 ArrayList + [pscustomobject]，类型明确、没有这个坑。
function Get-BlueColumnRuns {
    param([System.Drawing.Bitmap]$Bitmap, [int]$MaxX, [int]$MaxY)
    $runs = New-Object System.Collections.ArrayList
    $start = -1
    for ($x = 0; $x -lt $MaxX; $x++) {
        $n = 0
        for ($y = 0; $y -lt $MaxY; $y++) {
            $p = $Bitmap.GetPixel($x, $y)
            if (($p.B - [Math]::Max($p.R, $p.G)) -gt 40 -and $p.B -gt 140) { $n++ }
        }
        if ($n -gt 0) {
            if ($start -lt 0) { $start = $x }
        }
        elseif ($start -ge 0) {
            [void]$runs.Add([pscustomobject]@{ A = $start; B = ($x - 1) })
            $start = -1
        }
    }
    if ($start -ge 0) { [void]$runs.Add([pscustomobject]@{ A = $start; B = ($MaxX - 1) }) }
    return @($runs)
}

function Get-PixelDiffBox {
    param([System.Drawing.Bitmap]$A, [System.Drawing.Bitmap]$B, [string]$OutPath)
    $w = [Math]::Min($A.Width, $B.Width); $h = [Math]::Min($A.Height, $B.Height)
    $d = New-Object System.Drawing.Bitmap $w, $h
    $minX = $w; $minY = $h; $maxX = -1; $maxY = -1; $n = 0
    for ($y = 0; $y -lt $h; $y++) {
        for ($x = 0; $x -lt $w; $x++) {
            $pa = $A.GetPixel($x, $y); $pb = $B.GetPixel($x, $y)
            $delta = [Math]::Abs($pa.R - $pb.R) + [Math]::Abs($pa.G - $pb.G) + [Math]::Abs($pa.B - $pb.B)
            if ($delta -gt 24) {
                $d.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(255, 255, 0, 0)); $n++
                if ($x -lt $minX) { $minX = $x }; if ($y -lt $minY) { $minY = $y }
                if ($x -gt $maxX) { $maxX = $x }; if ($y -gt $maxY) { $maxY = $y }
            }
            else { $d.SetPixel($x, $y, [System.Drawing.Color]::FromArgb(40, 200, 200, 200)) }
        }
    }
    Save-Png -Bitmap $d -Path $OutPath
    $d.Dispose()
    if ($n -eq 0) { return $null }
    return [pscustomobject]@{ Count = $n; MinX = $minX; MinY = $minY; MaxX = $maxX; MaxY = $maxY }
}

# ---------------------------------------------------------------- 任务栏基线
$trayH = [CapIconShot]::FindWindowW('Shell_TrayWnd', $null)
if ($trayH -eq [IntPtr]::Zero) { Write-Warning '找不到 Shell_TrayWnd（没有任务栏？）' }
$trayR = New-Object CapIconShot+RECT
[void][CapIconShot]::GetWindowRect($trayH, [ref]$trayR)
$trayW = $trayR.Right - $trayR.Left; $trayHh = $trayR.Bottom - $trayR.Top
Write-Host ("[icon-shots] 任务栏 rect=($($trayR.Left),$($trayR.Top)) ${trayW}x${trayHh}")

function Shot-Tray {
    $bmp = New-Object System.Drawing.Bitmap $trayW, $trayHh
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($trayR.Left, $trayR.Top, 0, 0, $bmp.Size)
    $g.Dispose()
    return $bmp
}
$trayBefore = Shot-Tray
Save-Png -Bitmap $trayBefore -Path (Join-Path $OutDir 'taskbar-before.png')

# ---------------------------------------------------------------- 启动
$cfg = Join-Path $OutDir 'cfg'
if (Test-Path -LiteralPath $cfg) { Remove-Item -LiteralPath $cfg -Recurse -Force }
New-Item -ItemType Directory -Force -Path $cfg | Out-Null
$exeFull = (Resolve-Path -LiteralPath $Exe).Path
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exeFull
$psi.UseShellExecute = $false
# Windows PowerShell 5.1 没有 ProcessStartInfo.ArgumentList -> 自己拼带引号的命令行
$psi.Arguments = '--config-dir "' + $cfg + '"'
$proc = [System.Diagnostics.Process]::Start($psi)
Write-Host "[icon-shots] 已启动 pid=$($proc.Id) exe=$exeFull"

try {
    $hwnd = [IntPtr]::Zero
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $WaitMs) {
        Start-Sleep -Milliseconds 250
        $proc.Refresh()
        if ($proc.HasExited) { throw "进程提前退出（退出码 $($proc.ExitCode)）" }
        foreach ($h in [CapIconShot]::TopLevel([uint32]$proc.Id)) {
            $r = New-Object CapIconShot+RECT; [void][CapIconShot]::GetWindowRect($h, [ref]$r)
            $cr = New-Object CapIconShot+RECT; [void][CapIconShot]::GetClientRect($h, [ref]$cr)
            $w = $r.Right - $r.Left; $hh = $r.Bottom - $r.Top
            if ([CapIconShot]::IsWindowVisible($h) -and $w -gt 200 -and $hh -gt 200 -and (($hh - $cr.Bottom) -gt 10)) { $hwnd = $h }
        }
        if ($hwnd -ne [IntPtr]::Zero) { break }
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw "等了 $WaitMs ms 也没找到带标题栏的主窗口" }
    Write-Host ("[icon-shots] 主窗口 hwnd=0x{0:X} class='{1}' title='{2}'（{3} ms）" -f `
            [int64]$hwnd, [CapIconShot]::Cls($hwnd), [CapIconShot]::Txt($hwnd), $sw.ElapsedMilliseconds)
    Start-Sleep -Milliseconds 2500

    [void][CapIconShot]::SetWindowPos($hwnd, [IntPtr]::Zero, 40, 40, 1020, 720, 0x0040)
    Start-Sleep -Milliseconds 1500

    $wr = New-Object CapIconShot+RECT; [void][CapIconShot]::GetWindowRect($hwnd, [ref]$wr)
    $cr = New-Object CapIconShot+RECT; [void][CapIconShot]::GetClientRect($hwnd, [ref]$cr)
    $winW = $wr.Right - $wr.Left; $winH = $wr.Bottom - $wr.Top
    $titleH = $winH - $cr.Bottom
    Write-Host "[icon-shots] 窗口 ${winW}x${winH}，客户区 $($cr.Right)x$($cr.Bottom)，标题栏高 $titleH"

    foreach ($pair in @(@(0, 'wmgeticon'), @(1, 'big'), @(2, 'small2'))) {
        Write-Host ("[icon-shots] WM_GETICON {0} = 0x{1:X}" -f $pair[1], `
                [int64][CapIconShot]::SendMessageW($hwnd, 0x007F, [IntPtr]$pair[0], [IntPtr]0))
    }

    # PrintWindow(PW_RENDERFULLCONTENT=2)：把窗口自己画进我们的 DC，不需要抢前台
    $win = New-Object System.Drawing.Bitmap $winW, $winH
    $g = [System.Drawing.Graphics]::FromImage($win)
    $hdc = $g.GetHdc()
    [void][CapIconShot]::PrintWindow($hwnd, $hdc, 2)
    $g.ReleaseHdc($hdc)
    $g.Dispose()
    Save-Png -Bitmap $win -Path (Join-Path $OutDir 'window.png')

    # 标题栏左端整条（人眼先看它），再按"最右边那段够宽的蓝"裁出图标
    $stripW = [Math]::Min(200, $winW)
    $strip = New-Object System.Drawing.Bitmap $stripW, $titleH
    $gs = [System.Drawing.Graphics]::FromImage($strip)
    $gs.DrawImage($win, (New-Object System.Drawing.Rectangle 0, 0, $stripW, $titleH), `
            (New-Object System.Drawing.Rectangle 0, 0, $stripW, $titleH), [System.Drawing.GraphicsUnit]::Pixel)
    $gs.Dispose()
    Save-Png -Bitmap $strip -Path (Join-Path $OutDir 'titlebar-left-raw.png')
    (Scale-Nearest -Bitmap $strip -Factor 4).Save((Join-Path $OutDir 'titlebar-left-x4.png'), [System.Drawing.Imaging.ImageFormat]::Png)

    $runs = Get-BlueColumnRuns -Bitmap $win -MaxX ([Math]::Min(80, $winW)) -MaxY $titleH
    Write-Host ("[icon-shots] 标题栏蓝色列区间: {0}" -f (($runs | ForEach-Object { "$($_.A)-$($_.B)" }) -join ', '))
    # 最左那段可能是**邻居窗口**的图标（我们的窗口左边紧挨着别人的标题栏），取最右那段
    $icon = $runs | Where-Object { $_.A -ge 1 -and ($_.B - $_.A) -ge 5 } | Select-Object -Last 1
    if ($null -eq $icon) {
        Write-Warning '标题栏里没找到蓝色图标列区间——请人眼看 titlebar-left-raw.png'
    }
    else {
        $small = [int][CapIconShot]::GetSystemMetrics(49)   # SM_CXSMICON
        if ($small -le 0) { $small = 16 }
        $firstBlue = -1; $lastBlue = -1
        for ($y = 0; $y -lt $titleH; $y++) {
            for ($x = $icon.A; $x -le $icon.B; $x++) {
                $p = $win.GetPixel($x, $y)
                if (($p.B - [Math]::Max($p.R, $p.G)) -gt 40 -and $p.B -gt 140) {
                    if ($firstBlue -lt 0) { $firstBlue = $y }
                    $lastBlue = $y
                    break
                }
            }
        }
        if ($firstBlue -lt 0) { $firstBlue = 0; $lastBlue = $small - 1 }
        $mid = [int](($firstBlue + $lastBlue) / 2)
        $y0 = [Math]::Max(0, $mid - [int]($small / 2))
        $x0 = $icon.A; $x1 = $icon.B
        if (($x1 - $x0 + 1) -ne $small) { Write-Host "[icon-shots] 注意：蓝色列宽 $($x1 - $x0 + 1) != SM_CXSMICON $small" }
        $ib = New-Object System.Drawing.Bitmap $small, $small
        $gi = [System.Drawing.Graphics]::FromImage($ib)
        $gi.DrawImage($win, (New-Object System.Drawing.Rectangle 0, 0, $small, $small), `
                (New-Object System.Drawing.Rectangle $x0, $y0, $small, $small), [System.Drawing.GraphicsUnit]::Pixel)
        $gi.Dispose()
        Save-Png -Bitmap $ib -Path (Join-Path $OutDir 'icon-titlebar.png')
        (Scale-Nearest -Bitmap $ib -Factor 10).Save((Join-Path $OutDir 'icon-titlebar-x10.png'), [System.Drawing.Imaging.ImageFormat]::Png)
        Write-Host "[icon-shots] 标题栏图标 -> icon-titlebar.png（${small}x${small}，窗口内偏移 ($x0,$y0)）"
        $ib.Dispose()
    }
    $strip.Dispose(); $win.Dispose()

    # ---------------------------------------------------------------- 任务栏
    #
    # 怎么定位"我们的按钮"：启动前抓一条基线，启动后再抓一条，像素差里新出现的那块就是。
    # **但差集不保证只有我们的按钮**：时钟跳分钟、通知区域图标闪动都会让别的像素也变。
    # 所以这里不直接用整块差集的包围盒（那样会裁出一条几百像素宽的长条），而是把差集按列
    # 切成"连续的列段"，再取**最高**的一段（任务栏按钮高度 ≈ 整条任务栏）——那是唯一能跨满
    # 整条任务栏高度的东西。同时把整条"启动后"的任务栏也存下来，便于人眼核对。
    Start-Sleep -Milliseconds 800
    $trayAfter = Shot-Tray
    Save-Png -Bitmap $trayAfter -Path (Join-Path $OutDir 'taskbar-after.png')
    $box = Get-PixelDiffBox -A $trayBefore -B $trayAfter -OutPath (Join-Path $OutDir 'taskbar-diff.png')
    if ($null -eq $box) {
        Write-Warning '任务栏启动前后没有像素差——没有任务栏按钮？'
    }
    else {
        Write-Host ("[icon-shots] 任务栏差集：{0} px，包围盒 x[{1}..{2}] y[{3}..{4}]" -f $box.Count, $box.MinX, $box.MaxX, $box.MinY, $box.MaxY)
        # 算差集的"每列差多少像素"，再切成列段
        $diffBmp = New-Object System.Drawing.Bitmap (Join-Path $OutDir 'taskbar-diff.png')
        $colH = New-Object int[] $trayW
        for ($x = 0; $x -lt $trayW; $x++) {
            $n = 0
            for ($y = 0; $y -lt $trayHh; $y++) {
                $p = $diffBmp.GetPixel($x, $y)
                if ($p.R -gt 200 -and $p.G -lt 80 -and $p.B -lt 80) { $n++ }   # 差集里画的是纯红
            }
            $colH[$x] = $n
        }
        $diffBmp.Dispose()
        $seg = New-Object System.Collections.ArrayList
        $s = -1
        for ($x = 0; $x -lt $trayW; $x++) {
            if ($colH[$x] -gt 0) { if ($s -lt 0) { $s = $x } }
            elseif ($s -ge 0) { [void]$seg.Add([pscustomobject]@{ A = $s; B = ($x - 1); Peak = ($colH[$s..($x - 1)] | Measure-Object -Maximum).Maximum }); $s = -1 }
        }
        if ($s -ge 0) { [void]$seg.Add([pscustomobject]@{ A = $s; B = ($trayW - 1); Peak = ($colH[$s..($trayW - 1)] | Measure-Object -Maximum).Maximum }) }
        Write-Host ("[icon-shots] 差集列段: {0}" -f (($seg | ForEach-Object { "x[$($_.A)..$($_.B)] 最高 $($_.Peak)px 宽 $($_.B - $_.A + 1)" }) -join '; '))
        # 按钮 = 最高的一段；并列时取最靠中间的那段（新按钮通常靠近已有的一排）
        $btnSeg = $seg | Where-Object { ($_.B - $_.A + 1) -ge 14 -and ($_.B - $_.A + 1) -le 80 } | Sort-Object Peak -Descending | Select-Object -First 1
        if ($null -eq $btnSeg) { $btnSeg = $seg | Sort-Object Peak -Descending | Select-Object -First 1 }
        $x0 = [Math]::Max(0, $btnSeg.A - 8); $x1 = [Math]::Min($trayW - 1, $btnSeg.B + 8)
        $y0 = 0; $y1 = $trayHh - 1
        $btn = New-Object System.Drawing.Bitmap ($x1 - $x0 + 1), ($y1 - $y0 + 1)
        $gb = [System.Drawing.Graphics]::FromImage($btn)
        $gb.DrawImage($trayAfter, (New-Object System.Drawing.Rectangle 0, 0, $btn.Width, $btn.Height), `
                (New-Object System.Drawing.Rectangle $x0, $y0, $btn.Width, $btn.Height), [System.Drawing.GraphicsUnit]::Pixel)
        $gb.Dispose()
        Save-Png -Bitmap $btn -Path (Join-Path $OutDir 'icon-taskbar-area.png')
        (Scale-Nearest -Bitmap $btn -Factor 8).Save((Join-Path $OutDir 'icon-taskbar-area-x8.png'), [System.Drawing.Imaging.ImageFormat]::Png)
        Write-Host "[icon-shots] 任务栏按钮（列段 x[$($btnSeg.A)..$($btnSeg.B)]）-> icon-taskbar-area.png（$($btn.Width)x$($btn.Height)）"
        $btn.Dispose()
    }
    $trayAfter.Dispose()
}
finally {
    if ($null -ne $proc -and -not $proc.HasExited) { $proc.Kill(); $proc.WaitForExit(5000) | Out-Null }
    $trayBefore.Dispose()
    Start-Sleep -Milliseconds 1000
}
Write-Host "[icon-shots] 完成 -> $OutDir"
