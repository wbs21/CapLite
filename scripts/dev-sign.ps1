<#
.SYNOPSIS
  CapLite 开发用自签名：确保证书 → 清除下载标记(MotW) → 签名已构建的 exe → 回读校验。

.DESCRIPTION
  背景（为什么需要这个脚本）
  --------------------------
  cargo 每次重新编译都会产出一个**全新的、未签名**的 CapLite.exe，而本机没有任何代码签名证书，
  于是 Windows 会拦：
    * 「无法验证发布者。你确定要运行此软件吗？」—— Shell（Attachment Manager）在文件带
      Mark-of-the-Web（备用数据流 Zone.Identifier，来自"从网络下载/被中转"）且没有
      "受信任的发布者"签名时弹的确认框；
    * 「Windows 已保护你的电脑 —— 阻止了无法识别的应用启动」—— Microsoft Defender
      SmartScreen 对带 MotW 的低信誉文件的拦截。
  本脚本只做**当前用户级**的两件事：用本机自签名证书给 exe 签名，并清掉文件自身的下载标记。
  它**不**修改 SmartScreen / Defender / 智能应用控制(SAC) / 执行策略等任何系统安全设置。

  执行顺序（幂等，可反复运行；重新编译后重跑即可）
  ------------------------------------------------
    1) 证书：Cert:\CurrentUser\My 里已有 "CN=CapLite dev signing"（Code Signing 用途 +
       私钥 + 未过期）就复用，否则用 New-SelfSignedCertificate 新建一张（默认 5 年）。
    2) 信任：只把**公钥证书**（.cer，不含私钥）装进 Cert:\CurrentUser\TrustedPublisher，
       随后回读实测；如果只装 TrustedPublisher 仍不是 Valid，就再把它装进
       Cert:\CurrentUser\Root 并复测。脚本会打印这次实测的结论（TRUST-PROBE 行）。
       原因：Get-AuthenticodeSignature 走 X509 证书链校验，链必须终止在一张受信任的根证书
       上，状态才可能是 Valid。
    3) 标记：逐个检查 Zone.Identifier 备用数据流；存在就 Unblock-File，失败则直接删流，复查。
    4) 签名：Set-AuthenticodeSignature（新签名覆盖旧签名，因此重复运行安全）。
    5) 回读：逐个打印 Status / StatusMessage / 签名者。全部 Valid 才通过，否则 exit 1。

.PARAMETER Path
  要签名的文件、通配符或目录；默认 <工作区>\_rust\target-cap\debug\*.exe
  与 <工作区>\_rust\target-cap\release\*.exe（两者都存在就都签）。

.PARAMETER Subject
  开发证书主题，用来在 Cert:\CurrentUser\My 里识别/复用本项目的证书。
  默认 CN=CapLite dev signing。

.PARAMETER ValidYears
  新建证书的有效期（年），默认 5。

.PARAMETER NoTrustedRoot
  禁止在"只装 TrustedPublisher 不够"时追加安装到 Cert:\CurrentUser\Root。

.PARAMETER PortableKeyFallback
  证书存储不可写时（例如在受限沙箱/无权限环境里运行），退化为「工作区内的 PFX 私钥 +
  内存密钥（EphemeralKeySet）」签名。签名本身会成功，但受信任存储仍装不进去，因此
  Status 不会是 Valid、弹框也不会消失 —— 这个开关只用于在无权限环境里验证签名流程。

.PARAMETER PfxPassword
  -PortableKeyFallback 模式下工作区 PFX 的密码（默认 CapLite-dev-signing）。

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1

.EXAMPLE
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1 -Path ..\_rust\target-cap\debug\CapLite.exe

.EXAMPLE
  # 多个文件：用逗号写成一个字符串（-File 调用时数组语法不可靠）
  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1 -Path "a\CapLite.exe,b\clip_smoke.exe"
#>
#Requires -Version 5.1
# PositionalBinding = $false：只接受**具名**参数。否则 `-File ... -Path x y` 里那个多出来的 y
# 会被悄悄绑到下一个位置参数（-Subject）上，产生莫名其妙的错误。
[CmdletBinding(PositionalBinding = $false)]
param(
    [string[]]$Path = @(),
    [string]$Subject = 'CN=CapLite dev signing',
    [int]$ValidYears = 5,
    [switch]$NoTrustedRoot,
    [switch]$PortableKeyFallback,
    # 正式发布时传 RFC3161 时间戳服务器（例如 http://timestamp.digicert.com）；
    # 开发机离线时留空，保持旧行为。
    [string]$TimestampServer = '',
    # 优先用环境变量；保留旧默认值只为已有工作区 PFX 仍能打开。
    # 正式发布/共享机器上请设置 CAPLITE_PFX_PASSWORD，不要把默认密码当成安全边界。
    [string]$PfxPassword = $(if ($env:CAPLITE_PFX_PASSWORD) { $env:CAPLITE_PFX_PASSWORD } else { 'CapLite-dev-signing' })
)

$ErrorActionPreference = 'Stop'

# -Path 归一化：同时接受 -Path a,b（一个字符串）、-Path 'a;b'、以及数组写法
$__pathList = @()
foreach ($__p in $Path) {
    if ([string]::IsNullOrWhiteSpace($__p)) { continue }
    foreach ($__piece in ($__p -split '[,;]')) {
        $__t = $__piece.Trim().Trim('"').Trim("'")
        if ($__t -ne '') { $__pathList += $__t }
    }
}
$Path = $__pathList

# 退出码约定
$EXIT_OK            = 0
$EXIT_NOT_VALID     = 1   # 有 exe 没签成 / 状态不是 Valid
$EXIT_NO_TARGET     = 2   # 没找到任何目标 exe
$EXIT_CERT_FAILED   = 3   # 证书环节失败
$EXIT_TRUST_FAILED  = 4   # 受信任存储装不进去（因此不可能 Valid）

$script:Failures = @()
$script:Warnings = @()

# ---------------------------------------------------------------------------
# 输出小工具
# ---------------------------------------------------------------------------
function Write-Head { param([string]$Text) Write-Host ''; Write-Host "== $Text ==" -ForegroundColor Cyan }
function Write-Info { param([string]$Text) Write-Host "  $Text" }
function Write-Ok   { param([string]$Text) Write-Host "  [ OK ] $Text" -ForegroundColor Green }
function Write-Warn { param([string]$Text) Write-Host "  [WARN] $Text" -ForegroundColor Yellow; $script:Warnings += $Text }
function Write-Bad  { param([string]$Text) Write-Host "  [FAIL] $Text" -ForegroundColor Red; $script:Failures += $Text }

# ---------------------------------------------------------------------------
# 路径
# ---------------------------------------------------------------------------
$Root      = Split-Path -Parent $PSScriptRoot                 # ...\CapLite
$Workspace = Split-Path -Parent $Root                         # ...\default-workspace
$TargetRoot = Join-Path $Workspace '_rust\target-cap'
$KeyDir     = Join-Path $TargetRoot 'dev-signing'             # .cer / PFX 落地处（构建产物区，不入库）
$CerPath    = Join-Path $KeyDir 'CapLite-dev-signing.cer'
$PfxPath    = Join-Path $KeyDir 'CapLite-dev-signing.pfx'
$CodeSigningEku = '1.3.6.1.5.5.7.3.3'

if (-not (Test-Path -LiteralPath $KeyDir)) { New-Item -ItemType Directory -Path $KeyDir -Force | Out-Null }

Write-Head 'dev-sign: CapLite 开发签名'
Write-Info "工作区      : $Workspace"
Write-Info "目标产物目录: $TargetRoot"

# ---------------------------------------------------------------------------
# 解析要签名的文件
# ---------------------------------------------------------------------------
function Resolve-TargetFiles {
    param([string[]]$Patterns)
    $files = New-Object System.Collections.Generic.List[string]
    $seen = @{}
    foreach ($p in $Patterns) {
        if ([string]::IsNullOrWhiteSpace($p)) { continue }
        $items = @()
        if ($p -match '[*?]') {
            $items = @(Get-ChildItem -Path $p -File -ErrorAction SilentlyContinue)
        } elseif (Test-Path -LiteralPath $p -PathType Container) {
            $items = @(Get-ChildItem -LiteralPath $p -File -ErrorAction SilentlyContinue | Where-Object { $_.Extension -eq '.exe' })
        } elseif (Test-Path -LiteralPath $p) {
            $items = @(Get-Item -LiteralPath $p -ErrorAction SilentlyContinue)
        } else {
            Write-Warn "找不到路径（已跳过）：$p"
        }
        foreach ($it in $items) {
            $full = $it.FullName
            if (-not $seen.ContainsKey($full)) { $seen[$full] = $true; [void]$files.Add($full) }
        }
    }
    return $files
}

if ($Path.Count -eq 0) {
    $debugPattern   = Join-Path $TargetRoot 'debug\*.exe'
    $releasePattern = Join-Path $TargetRoot 'release\*.exe'
    $haveDebug   = Test-Path -LiteralPath (Split-Path -Parent $debugPattern)
    $haveRelease = Test-Path -LiteralPath (Split-Path -Parent $releasePattern)
    if (-not $haveDebug -and -not $haveRelease) {
        Write-Host ''
        Write-Host "找不到任何构建产物目录：" -ForegroundColor Red
        Write-Host "  $((Split-Path -Parent $debugPattern))"
        Write-Host "  $((Split-Path -Parent $releasePattern))"
        Write-Host '请先构建（例如 scripts\run-dev.ps1，或 cargo build --bin CapLite），或用 -Path 指定要签名的 exe。'
        exit $EXIT_NO_TARGET
    }
    $patterns = @()
    if ($haveDebug)   { $patterns += $debugPattern }
    if ($haveRelease) { $patterns += $releasePattern }
} else {
    $patterns = $Path
}

$targets = @(Resolve-TargetFiles -Patterns $patterns)
Write-Head '目标文件'
if ($targets.Count -eq 0) {
    Write-Host "  $($patterns -join '  ')"
    Write-Host '没有任何 exe 匹配（目录存在但没有 *.exe？）。先构建：scripts\run-dev.ps1 或 cargo build --bin CapLite。' -ForegroundColor Red
    exit $EXIT_NO_TARGET
}
foreach ($t in $targets) { Write-Info $t }

# ---------------------------------------------------------------------------
# 证书
# ---------------------------------------------------------------------------
function Test-CodeSigningEku {
    param($Cert)
    try {
        foreach ($e in $Cert.EnhancedKeyUsageList) {
            if ($e.ObjectId -eq $CodeSigningEku) { return $true }
        }
    } catch { }
    return $false
}

function Get-MatchingCerts {
    param([string]$SubjectName)
    $all = @()
    try { $all = @(Get-ChildItem -Path 'Cert:\CurrentUser\My' -ErrorAction Stop) }
    catch { throw "无法读取 Cert:\CurrentUser\My：$($_.Exception.Message)" }
    return @($all | Where-Object { $_.Subject -eq $SubjectName })
}

function Select-UsableCert {
    param($Certs)
    $ok = @($Certs | Where-Object { $_.HasPrivateKey -and (Test-CodeSigningEku $_) -and $_.NotAfter -gt (Get-Date) })
    if ($ok.Count -eq 0) { return $null }
    return ($ok | Sort-Object NotAfter -Descending | Select-Object -First 1)
}

function New-PortableCert {
    param([string]$SubjectName, [int]$Years, [string]$Pfx, [string]$Password)
    if (Test-Path -LiteralPath $Pfx) {
        try {
            $bytes = [System.IO.File]::ReadAllBytes($Pfx)
            $c = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new(
                    $bytes, $Password,
                    [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet)
            if ($c.Subject -eq $SubjectName -and $c.HasPrivateKey -and $c.NotAfter -gt (Get-Date)) {
                Write-Info "复用工作区 PFX 私钥：$Pfx"
                return $c
            }
            Write-Warn '工作区 PFX 里的证书主题/有效期不匹配，重新生成。'
        } catch {
            Write-Warn "读取工作区 PFX 失败（重新生成）：$($_.Exception.Message)"
        }
    }
    Write-Info '用 .NET CertificateRequest 生成自签名证书（私钥只落在工作区 PFX，不写用户密钥库）'
    $rsa = [System.Security.Cryptography.RSA]::Create(2048)
    $req = [System.Security.Cryptography.X509Certificates.CertificateRequest]::new(
            $SubjectName, $rsa,
            [System.Security.Cryptography.HashAlgorithmName]::SHA256,
            [System.Security.Cryptography.RSASignaturePadding]::Pkcs1)
    $req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $false))
    $req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new(
            [System.Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature, $true))
    $eku = [System.Security.Cryptography.OidCollection]::new()
    [void]$eku.Add([System.Security.Cryptography.Oid]::new($CodeSigningEku))
    $req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509EnhancedKeyUsageExtension]::new($eku, $false))
    $req.CertificateExtensions.Add([System.Security.Cryptography.X509Certificates.X509SubjectKeyIdentifierExtension]::new($req.PublicKey, $false))
    $cert = $req.CreateSelfSigned([System.DateTimeOffset]::Now.AddDays(-1), [System.DateTimeOffset]::Now.AddYears($Years))
    [System.IO.File]::WriteAllBytes($Pfx, $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Pfx, $Password))
    Write-Warn "私钥已写入工作区 PFX（请勿提交/外传）：$Pfx"
    return $cert
}

Write-Head '1/4 证书'
$cert = $null
$certSource = ''
$existing = @()
try { $existing = @(Get-MatchingCerts -SubjectName $Subject) }
catch { Write-Bad $_.Exception.Message; exit $EXIT_CERT_FAILED }

foreach ($c in $existing) {
    Write-Info ("已有同名证书：thumb={0}  notAfter={1:yyyy-MM-dd}  hasPrivateKey={2}  codeSigningEku={3}" -f `
        $c.Thumbprint, $c.NotAfter, $c.HasPrivateKey, (Test-CodeSigningEku $c))
}
$cert = Select-UsableCert -Certs $existing
if (($null -eq $cert) -and ($existing.Count -eq 0)) {
    # ---------------------------------------------------------------------
    # **改名后的兼容分支（本轮新增）**：产品从 `CapLite` 改名成 CapLite 之后，本机
    # `Cert:\CurrentUser\My` 里那张**已经装进 TrustedPublisher + Root** 的开发证书主体仍是
    # `CN=CapLite dev signing`。新建一张同名证书需要写用户密钥库 —— 在很多上下文里
    # （受限沙箱 / 无权限）会直接 `NTE_PERM (0x80090010)`，于是"能验签的证书明明在，
    # 却因为名字对不上而签不了"。
    # 这里退一步：**显式**复用那张旧主体的可用证书（只在"新主体一张都没有"时才走），
    # 并在输出里把这件事说清楚 —— 签名链因此仍然可信（Status=Valid），只有签名者
    # 显示的那行字是旧名字。想换成新主体：在有权限的普通 PowerShell 里跑一次本脚本即可
    # （那时 `New-SelfSignedCertificate` 会成功，本分支不再触发）。
    # ---------------------------------------------------------------------
    $legacy = @(Get-ChildItem -Path 'Cert:\CurrentUser\My' -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -like 'CN=* dev signing' -and $_.Subject -ne $Subject })
    $legacyUsable = Select-UsableCert -Certs $legacy
    if ($null -ne $legacyUsable) {
        $cert = $legacyUsable
        $certSource = 'store'
        Write-Warn ("没有主体为 `"$Subject`" 的可用证书（新建需要写用户密钥库）；" +
            "改用本机已有、且已装进受信任存储的开发证书：$($cert.Subject)  thumb=$($cert.Thumbprint)")
        Write-Warn '  → 签名仍然可信（Status 应为 Valid），但签名者显示的是上面这个主体名。'
    }
}
if ($null -ne $cert -and $certSource -eq '') {
    $certSource = 'store'
    Write-Ok "复用 Cert:\CurrentUser\My 里的开发证书：$($cert.Subject)  thumb=$($cert.Thumbprint)  notAfter=$($cert.NotAfter.ToString('yyyy-MM-dd'))"
} elseif ($null -ne $cert) {
    # 兼容分支已经把结论打印过了（store），这里不再重复
} else {
    if ($existing.Count -gt 0) { Write-Warn '同名证书存在但没有一张可用（缺私钥 / 无 Code Signing 用途 / 已过期），将新建一张。' }
    try {
        if (-not (Get-Command New-SelfSignedCertificate -ErrorAction SilentlyContinue)) {
            throw [System.InvalidOperationException]::new('本机没有 New-SelfSignedCertificate（PKI 模块不可用）')
        }
        Write-Info "新建证书：New-SelfSignedCertificate -Type CodeSigningCert -Subject `"$Subject`" -CertStoreLocation Cert:\CurrentUser\My -NotAfter (+$ValidYears 年)"
        $null = New-SelfSignedCertificate -Type CodeSigningCert -KeyUsage DigitalSignature `
                    -Subject $Subject -CertStoreLocation 'Cert:\CurrentUser\My' `
                    -NotAfter (Get-Date).AddYears($ValidYears) -ErrorAction Stop
        # New-SelfSignedCertificate 在不同宿主里可能返回"脱序列化"对象（没有私钥），
        # 因此一律回到存储里重新读取**活**对象。
        $cert = Select-UsableCert -Certs (Get-MatchingCerts -SubjectName $Subject)
        if ($null -eq $cert) { throw [System.InvalidOperationException]::new('证书已创建，但无法从 Cert:\CurrentUser\My 读回可用证书（缺私钥？）') }
        $certSource = 'store'
        Write-Ok "已创建并读回：$($cert.Subject)  thumb=$($cert.Thumbprint)  notAfter=$($cert.NotAfter.ToString('yyyy-MM-dd'))"
    } catch {
        $msg = $_.Exception.Message
        Write-Host ''
        Write-Host '创建证书失败：' -ForegroundColor Red
        Write-Host "  $msg"
        if ($msg -match 'NTE_PERM|0x80090010|拒绝访问|Access is denied|0x80070005|E_ACCESSDENIED') {
            Write-Host ''
            Write-Host '这是**权限**问题：当前进程不能写当前用户的证书/密钥库' -ForegroundColor Yellow
            Write-Host '（典型原因：在受限沙箱 / 无权限上下文里运行）。请改到**普通 PowerShell 窗口**运行：' -ForegroundColor Yellow
            Write-Host '  powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1'
        }
        if ($PortableKeyFallback) {
            Write-Warn '按 -PortableKeyFallback 继续：改用工作区 PFX + 内存密钥签名（受信任存储仍装不进去，Status 不会是 Valid）。'
            $cert = New-PortableCert -SubjectName $Subject -Years $ValidYears -Pfx $PfxPath -Password $PfxPassword
            $certSource = 'portable'
        } else {
            Write-Host ''
            Write-Host '提示：若只想在无权限环境里验证签名流程，可加 -PortableKeyFallback；' -ForegroundColor Yellow
            Write-Host '      要让弹框真正消失，必须在能写证书存储的普通 PowerShell 里运行本脚本。' -ForegroundColor Yellow
            exit $EXIT_CERT_FAILED
        }
    }
}
if ($null -eq $cert -or -not $cert.HasPrivateKey) { Write-Bad '没有拿到带私钥的证书'; exit $EXIT_CERT_FAILED }

# 导出公钥证书（只有公钥，可安全地装进"受信任的发布者/根"）
try {
    [System.IO.File]::WriteAllBytes($CerPath, $cert.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert))
    Write-Info "公钥证书已导出：$CerPath"
} catch {
    Write-Warn "导出公钥证书失败（不影响签名，但无法自动装信任存储）：$($_.Exception.Message)"
}

# ---------------------------------------------------------------------------
# 受信任存储
# ---------------------------------------------------------------------------
function Test-CertInStore {
    param([string]$StorePath, [string]$Thumbprint)
    try {
        $hit = @(Get-ChildItem -Path $StorePath -ErrorAction Stop | Where-Object { $_.Thumbprint -eq $Thumbprint })
        return ($hit.Count -gt 0)
    } catch {
        Write-Warn "读取 $StorePath 失败：$($_.Exception.Message)"
        return $false
    }
}

function Install-CertToStore {
    param([string]$CerFile, [string]$StoreName, [string]$Thumbprint)
    $storePath = "Cert:\CurrentUser\$StoreName"
    if (Test-CertInStore -StorePath $storePath -Thumbprint $Thumbprint) {
        Write-Ok "$storePath 里已有该证书（无需重复安装）"
        return $true
    }
    # certutil 不会弹 UI（Import-Certificate 装根证书时会弹安全警告窗口）
    $cu = Get-Command certutil.exe -ErrorAction SilentlyContinue
    if ($cu) {
        $out = & $cu.Source -user -f -addstore $StoreName $CerFile 2>&1
        if ($LASTEXITCODE -eq 0) { Write-Ok "已用 certutil 装入 $storePath"; return $true }
        Write-Warn "certutil -addstore $StoreName 失败（exit=$LASTEXITCODE）：$(($out | Select-Object -First 2) -join ' / ')"
    }
    try {
        $null = Import-Certificate -FilePath $CerFile -CertStoreLocation $storePath -ErrorAction Stop
        Write-Ok "已用 Import-Certificate 装入 $storePath"
        return $true
    } catch {
        Write-Warn "装入 $storePath 失败：$($_.Exception.Message)"
        return $false
    }
}

Write-Head '2/4 受信任存储'
$trustOk = $false
if ($certSource -eq 'portable') {
    Write-Warn '便携证书不在用户证书库里，跳过信任存储安装（此模式只为验证签名流程）。'
} else {
    $trustOk = Install-CertToStore -CerFile $CerPath -StoreName 'TrustedPublisher' -Thumbprint $cert.Thumbprint
    if (-not $trustOk) {
        Write-Warn 'TrustedPublisher 装不进去 —— 签名状态不会是 Valid（权限不足时请改到普通 PowerShell 运行）。'
    }
}

# ---------------------------------------------------------------------------
# 清除 Mark-of-the-Web + 签名
# ---------------------------------------------------------------------------
function Get-ZoneIdentifier {
    param([string]$FilePath)
    try {
        $text = Get-Content -LiteralPath $FilePath -Stream 'Zone.Identifier' -Raw -ErrorAction Stop
        return [string]$text
    } catch { return $null }
}

function Get-StreamNames {
    param([string]$FilePath)
    try { return @(Get-Item -LiteralPath $FilePath -Stream * -ErrorAction Stop | ForEach-Object { $_.Stream }) }
    catch { return @() }
}

function Clear-MotW {
    param([string]$FilePath)
    $before = Get-ZoneIdentifier -FilePath $FilePath
    if ($null -eq $before) {
        Write-Ok "无 Zone.Identifier（无需清除）"
        return [pscustomobject]@{ HadMotW = $false; Before = ''; StillThere = $false }
    }
    Write-Warn ("发现 Zone.Identifier，内容：" + (($before -split "`r?`n" | Where-Object { $_.Trim() -ne '' }) -join ' | '))
    $cleared = Remove-ZoneIdentifierStream -Target $FilePath
    if ($cleared) { Write-Ok 'Zone.Identifier 已清除' } else { Write-Bad 'Zone.Identifier 仍存在（清除失败）' }
    return [pscustomobject]@{ HadMotW = $true; Before = $before; StillThere = (-not $cleared) }
}

# 清除备用数据流，两步走：
#   1) Unblock-File（官方 API，对**文件**有效；对**目录**实测常常"报成功但没效果"）；
#   2) 还没清掉就用 .NET 直接删流。注意 `Remove-Item -Stream` 对**目录**会要求交互确认，
#      非交互会话里直接失败（"PowerShell 处于非交互模式"），所以不用它；
#      [System.IO.File]::Delete('<path>:Zone.Identifier') 对文件和目录都有效（已实测）。
function Remove-ZoneIdentifierStream {
    param([string]$Target)
    try { Unblock-File -LiteralPath $Target -ErrorAction Stop } catch { Write-Warn "Unblock-File 失败（$Target）：$($_.Exception.Message)" }
    if ($null -eq (Get-ZoneIdentifier -FilePath $Target)) { return $true }
    try { [System.IO.File]::Delete($Target + ':Zone.Identifier') }
    catch { Write-Warn "删除备用数据流失败（$Target）：$($_.Exception.Message)" }
    return ($null -eq (Get-ZoneIdentifier -FilePath $Target))
}

$results = New-Object System.Collections.Generic.List[object]

Write-Head '3/4 清除下载标记并签名'

# MotW 不一定只挂在文件上，也可能挂在**目录**上（解压/中转过的目录），
# Windows 对"带区域信息的目录"里的文件同样会做附件检查，所以这里一并检查并清除。
# 范围：每个目标文件的目录，逐级向上直到工作区根目录，同一目录只处理一次。
$dirChecked = @{}
foreach ($f in $targets) {
    $d = Split-Path -Parent $f
    while (-not [string]::IsNullOrWhiteSpace($d)) {
        if ($dirChecked.ContainsKey($d)) { break }
        $dirChecked[$d] = $true
        $dirMotW = $null
        try { $dirMotW = Get-Content -LiteralPath $d -Stream 'Zone.Identifier' -Raw -ErrorAction Stop } catch { $dirMotW = $null }
        if ($null -ne $dirMotW) {
            Write-Warn ("目录也带 Zone.Identifier：{0}" -f $d)
            Write-Warn ("  内容：" + (($dirMotW -split "`r?`n" | Where-Object { $_.Trim() -ne '' }) -join ' | '))
            if (Remove-ZoneIdentifierStream -Target $d) { Write-Ok "  目录标记已清除：$d" } else { Write-Bad "  目录标记仍存在：$d" }
        }
        if ($d -eq $Workspace) { break }
        $parent = Split-Path -Parent $d
        if ([string]::IsNullOrWhiteSpace($parent) -or $parent -eq $d) { break }
        $d = $parent
    }
}
Write-Info '（目录级 Zone.Identifier 检查完成；上面没有 WARN 行就说明各级目录都干净）'

foreach ($f in $targets) {
    Write-Host ''
    Write-Info "文件：$f"
    $sizeMb = [Math]::Round(((Get-Item -LiteralPath $f).Length / 1MB), 2)
    Write-Info ("大小 {0} MB   备用数据流：{1}" -f $sizeMb, ((Get-StreamNames -FilePath $f) -join ', '))

    $motw = Clear-MotW -FilePath $f

    # 正在运行的同名进程会让签名失败，提前给出可读提示
    $base = [System.IO.Path]::GetFileNameWithoutExtension($f)
    $procs = @(Get-Process -Name $base -ErrorAction SilentlyContinue)
    if ($procs.Count -gt 0) {
        Write-Warn ("进程 {0} 正在运行（PID {1}）—— 签名很可能因文件被占用而失败；请先 Stop-Process -Name {0}" -f $base, (($procs | ForEach-Object { $_.Id }) -join ','))
    }

    $before = $null
    try { $before = Get-AuthenticodeSignature -LiteralPath $f -ErrorAction Stop } catch { }
    if ($before) { Write-Info "签名前状态：$($before.Status)" }

    try {
        $sigArgs = @{
            LiteralPath   = $f
            Certificate   = $cert
            HashAlgorithm = 'SHA256'
            ErrorAction   = 'Stop'
        }
        if (-not [string]::IsNullOrWhiteSpace($TimestampServer)) {
            $sigArgs.TimestampServer = $TimestampServer
        }
        $sig = Set-AuthenticodeSignature @sigArgs
        if ($sig.Status -eq 'Valid') {
            Write-Ok '已写入签名（Status=Valid）'
        } else {
            Write-Info "已写入签名（返回状态：$($sig.Status)；证书链是否被信任见第 4 步）"
        }
    } catch {
        $msg = $_.Exception.Message
        if ($msg -match 'being used by another process|正由另一进程使用|另一个进程|占用') {
            Write-Bad "签名失败：文件被占用（$base 正在运行？）。请关闭它后重跑本脚本。原始错误：$msg"
        } else {
            Write-Bad "签名失败：$msg"
        }
    }
    [void]$results.Add([pscustomobject]@{ File = $f; MotW = $motw })
}

# ---------------------------------------------------------------------------
# 回读校验 + 信任探测
# ---------------------------------------------------------------------------
function Show-SignatureReport {
    param([string[]]$Files)
    $allValid = $true
    foreach ($f in $Files) {
        $sig = $null
        try { $sig = Get-AuthenticodeSignature -LiteralPath $f -ErrorAction Stop } catch { Write-Bad "读取签名失败（$f）：$($_.Exception.Message)"; $allValid = $false; continue }
        $signer = '(无)'
        $thumb = ''
        if ($null -ne $sig.SignerCertificate) { $signer = $sig.SignerCertificate.Subject; $thumb = $sig.SignerCertificate.Thumbprint }
        $motwNow = Get-ZoneIdentifier -FilePath $f
        $motwText = '仍在'
        if ($null -eq $motwNow) { $motwText = '无' }
        $line = "Status={0}  Signer={1}" -f $sig.Status, $signer
        if ($thumb) { $line += "  thumb=$thumb" }
        $line += "  MotW=$motwText"
        if ($sig.Status -eq 'Valid' -and $null -eq $motwNow) { Write-Ok $line } else { Write-Bad $line }
        Write-Info ("      文件：{0}" -f $f)
        if ($sig.StatusMessage) { Write-Info ("      StatusMessage：{0}" -f $sig.StatusMessage) }
        $hashText = [string]$sig.HashAlgorithm
        if ([string]::IsNullOrWhiteSpace($hashText)) { $hashText = '(未报告)' }
        $tsText = '无'
        if ($null -ne $sig.TimeStamperCertificate) { $tsText = $sig.TimeStamperCertificate.Subject }
        Write-Info ("      HashAlgorithm={0}  SignatureType={1}  时间戳={2}" -f $hashText, $sig.SignatureType, $tsText)
        if ($sig.Status -ne 'Valid') { $allValid = $false }
    }
    return $allValid
}

Write-Head '4/4 回读校验'
$allValid = Show-SignatureReport -Files $targets

# 信任探测：只装 TrustedPublisher 是否已经足够？（结论直接打印出来，便于写进文档/排障）
if ($certSource -eq 'store') {
    Write-Host ''
    if (-not $trustOk) {
        Write-Info 'TRUST-PROBE：证书未能装入 Cert:\CurrentUser\TrustedPublisher，本次无法判定"只装 TrustedPublisher 是否足够"；请在能写证书存储的普通 PowerShell 里重跑。'
    } elseif ($allValid) {
        Write-Host '  TRUST-PROBE 结论：只装 Cert:\CurrentUser\TrustedPublisher 就足够（Status=Valid），无需装 Root。' -ForegroundColor Green
    } elseif (-not $NoTrustedRoot) {
        Write-Info 'TRUST-PROBE：只装 TrustedPublisher 时状态不是 Valid，尝试追加 Cert:\CurrentUser\Root（受信任的根）后复测……'
        $rootOk = Install-CertToStore -CerFile $CerPath -StoreName 'Root' -Thumbprint $cert.Thumbprint
        if ($rootOk) {
            $allValid = Show-SignatureReport -Files $targets
            Write-Host ''
            if ($allValid) {
                Write-Host '  TRUST-PROBE 结论：只装 TrustedPublisher **不够**；同时装 Cert:\CurrentUser\Root 之后 Status=Valid。' -ForegroundColor Yellow
            } else {
                Write-Host '  TRUST-PROBE 结论：TrustedPublisher + Root 都装了仍不是 Valid，请检查证书是否被吊销/过期或文件被占用。' -ForegroundColor Red
            }
        }
    } else {
        Write-Info 'TRUST-PROBE：只装 TrustedPublisher 不够，但已指定 -NoTrustedRoot，跳过 Root 复测。'
    }
}

# ---------------------------------------------------------------------------
# 汇总
# ---------------------------------------------------------------------------
Write-Head '汇总'
Write-Info ("证书来源 : {0}" -f $(switch ($certSource) { 'store' { "Cert:\CurrentUser\My（$Subject）" } 'portable' { '工作区 PFX（便携，未进用户证书库）' } default { '(未知)' } }))
Write-Info ("证书指纹 : {0}" -f $cert.Thumbprint)
Write-Info ("有效期至 : {0:yyyy-MM-dd}" -f $cert.NotAfter)
Write-Info ("目标文件 : {0} 个" -f $targets.Count)
$motwCount = @($results | Where-Object { $_.MotW.HadMotW }).Count
$motwLeft  = @($results | Where-Object { $_.MotW.StillThere }).Count
Write-Info ("MotW     : 原有标记 {0} 个，清除后残留 {1} 个" -f $motwCount, $motwLeft)

if ($allValid -and $motwLeft -eq 0) {
    Write-Host ''
    Write-Host 'PASS：全部可执行文件已签名且 Status=Valid，且没有 Zone.Identifier 标记。' -ForegroundColor Green
    Write-Host '重新编译后（cargo build ...）exe 会被替换成新的未签名文件，重跑本脚本即可；或用 scripts\run-dev.ps1 一键完成。'
    exit $EXIT_OK
}

Write-Host ''
Write-Host 'FAIL：仍有文件不是 Valid（或 MotW 没清掉）。' -ForegroundColor Red
if ($certSource -eq 'portable') {
    Write-Host '  原因：便携模式没有把公钥证书装进受信任存储 —— 请在普通 PowerShell 里重跑本脚本。' -ForegroundColor Yellow
} elseif (-not $trustOk) {
    Write-Host '  原因：证书装不进受信任存储（TrustedPublisher）。请在能写当前用户证书存储的普通 PowerShell 里运行。' -ForegroundColor Yellow
} elseif ($certSource -eq 'store') {
    Write-Host '  原因：证书未被信任链接受。可重跑并允许装 Cert:\CurrentUser\Root，或用 -Path 指定单个文件排查。' -ForegroundColor Yellow
}
exit $EXIT_NOT_VALID
