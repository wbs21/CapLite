# extract-icon.ps1 —— 从 exe 的**资源节**里把嵌入的图标按各尺寸取出来，作为"图标已嵌入"的硬证据。
#
# 为什么不用 `[System.Drawing.Icon]::ExtractAssociatedIcon()`：它只返回**一个** 32×32，
# 拿不到"多尺寸都在"这件事。这里走 Win32 资源 API：
#   LoadLibraryExW(LOAD_LIBRARY_AS_DATAFILE) → FindResource(RT_GROUP_ICON=14)
#   → 解析 GRPICONDIR 得到每个尺寸的 RT_ICON id → CreateIconFromResourceEx 还原 HICON
#   → System.Drawing.Icon.ToBitmap().Save(png)
#
# 用法：
#   pwsh -NoProfile -File scripts\extract-icon.ps1 -Exe <exe> -OutDir <目录>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$OutDir
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

$sig = @'
using System;
using System.Runtime.InteropServices;

public static class CapIconRes {
    public const uint LOAD_LIBRARY_AS_DATAFILE = 0x00000002;

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    public static extern IntPtr LoadLibraryExW(string lpLibFileName, IntPtr hFile, uint dwFlags);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool FreeLibrary(IntPtr hModule);
    [DllImport("kernel32.dll", SetLastError = true, EntryPoint = "FindResourceW", CharSet = CharSet.Unicode)]
    public static extern IntPtr FindResourceW(IntPtr hModule, IntPtr lpName, IntPtr lpType);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr LoadResource(IntPtr hModule, IntPtr hResInfo);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern IntPtr LockResource(IntPtr hResData);
    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern uint SizeofResource(IntPtr hModule, IntPtr hResInfo);
    [DllImport("user32.dll", SetLastError = true)]
    public static extern IntPtr CreateIconFromResourceEx(byte[] presbits, uint dwResSize,
        bool fIcon, uint dwVer, int cxDesired, int cyDesired, uint flags);

    /// 读出 RT_GROUP_ICON 的原始字节（GRPICONDIR）。
    public static byte[] ReadGroupIcon(string exe, int groupId) {
        IntPtr h = LoadLibraryExW(exe, IntPtr.Zero, LOAD_LIBRARY_AS_DATAFILE);
        if (h == IntPtr.Zero) throw new Exception("LoadLibraryExW failed: " + Marshal.GetLastWin32Error());
        try {
            IntPtr info = FindResourceW(h, new IntPtr(groupId), new IntPtr(14)); // 14 = RT_GROUP_ICON
            if (info == IntPtr.Zero) throw new Exception("RT_GROUP_ICON " + groupId + " not found");
            uint size = SizeofResource(h, info);
            IntPtr data = LoadResource(h, info);
            IntPtr p = LockResource(data);
            if (p == IntPtr.Zero) throw new Exception("LockResource failed");
            byte[] buf = new byte[size];
            Marshal.Copy(p, buf, 0, (int)size);
            return buf;
        } finally { FreeLibrary(h); }
    }

    /// 读出某个序号 RT_ICON 的原始字节（BITMAPINFOHEADER + 像素/AND 掩码）。
    public static byte[] ReadIcon(string exe, int iconId) {
        IntPtr h = LoadLibraryExW(exe, IntPtr.Zero, LOAD_LIBRARY_AS_DATAFILE);
        if (h == IntPtr.Zero) throw new Exception("LoadLibraryExW failed: " + Marshal.GetLastWin32Error());
        try {
            IntPtr info = FindResourceW(h, new IntPtr(iconId), new IntPtr(3)); // 3 = RT_ICON
            if (info == IntPtr.Zero) throw new Exception("RT_ICON " + iconId + " not found");
            uint size = SizeofResource(h, info);
            IntPtr data = LoadResource(h, info);
            IntPtr p = LockResource(data);
            if (p == IntPtr.Zero) throw new Exception("LockResource failed");
            byte[] buf = new byte[size];
            Marshal.Copy(p, buf, 0, (int)size);
            return buf;
        } finally { FreeLibrary(h); }
    }
}
'@
Add-Type -TypeDefinition $sig -Language CSharp

$exeFull = (Resolve-Path -LiteralPath $Exe).Path
if (-not (Test-Path -LiteralPath $OutDir)) { New-Item -ItemType Directory -Force -Path $OutDir | Out-Null }
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

Write-Host ("[icon] exe = {0}" -f $exeFull)

# --- 1) 解析 GRPICONDIR ---
$grp = [CapIconRes]::ReadGroupIcon($exeFull, 1)
$count = [BitConverter]::ToUInt16($grp, 4)      # idCount
Write-Host ("[icon] RT_GROUP_ICON #1: {0} 个条目（{1} 字节）" -f $count, $grp.Length)

$entries = @()
for ($i = 0; $i -lt $count; $i++) {
    $off = 6 + 14 * $i
    $w = $grp[$off]                                # 0 表示 256
    $h = $grp[$off + 1]
    if ($w -eq 0) { $w = 256 }
    if ($h -eq 0) { $h = 256 }
    $bytesInRes = [BitConverter]::ToUInt32($grp, $off + 8)
    $id = [BitConverter]::ToUInt16($grp, $off + 12)
    $entries += [pscustomobject]@{ Width = $w; Height = $h; BytesInRes = $bytesInRes; Id = $id }
}

$results = @()
foreach ($e in ($entries | Sort-Object Width -Descending)) {
    $raw = [CapIconRes]::ReadIcon($exeFull, $e.Id)
    $hicon = [CapIconRes]::CreateIconFromResourceEx($raw, [uint32]$raw.Length, $true, 0x00030000, $e.Width, $e.Height, 0)
    if ($hicon -eq [IntPtr]::Zero) {
        $results += [pscustomobject]@{ Size = "$($e.Width)x$($e.Height)"; IconId = $e.Id; BytesInRes = $e.BytesInRes; Png = '-'; PngBytes = 0; Note = 'CreateIconFromResourceEx 失败' }
        continue
    }
    $ic = [System.Drawing.Icon]::FromHandle($hicon)
    $png = Join-Path $OutDir ("caplite-exe-icon-{0}.png" -f $e.Width)
    $ic.ToBitmap().Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
    $len = (Get-Item -LiteralPath $png).Length
    $results += [pscustomobject]@{ Size = "$($e.Width)x$($e.Height)"; IconId = $e.Id; BytesInRes = $e.BytesInRes; Png = (Split-Path $png -Leaf); PngBytes = $len; Note = 'ok' }
}

Write-Host ''
Write-Host '=== 从 exe 资源节提取出的图标条目 ==='
$results | Format-Table -AutoSize
$ok = ($results | Where-Object Note -eq 'ok').Count
Write-Host ("[icon] 成功提取 {0}/{1} 个尺寸 → {2}" -f $ok, $results.Count, $OutDir)
if ($ok -eq 0) { exit 1 }
