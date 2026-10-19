# 开发自签名 / 消除「无法验证发布者」与 SmartScreen 弹框

> 适用：`CapLite` 在本机（Windows）用 `cargo` 自己编译、自己运行的开发场景。
> 相关脚本：[scripts/dev-sign.ps1](../scripts/dev-sign.ps1)、[scripts/run-dev.ps1](../scripts/run-dev.ps1)。
> 本方案**只影响当前用户**（`Cert:\CurrentUser\*`、目标 exe 自身的备用数据流、启动方式），
> **不改** SmartScreen、Defender、智能应用控制、执行策略等任何系统安全设置。

---

## 1. 两个框、两种原因

| 弹框 | 触发组件 | 必要条件 |
| --- | --- | --- |
| 「无法验证发布者。你确定要运行此软件吗？」 | Shell / **ShellExecuteEx** 路径上的 Attachment Manager 安全警告 | ① 启动走 Shell（`Start-Process` 默认、资源管理器双击、快捷方式）；② 文件带 **Mark-of-the-Web**（`Zone.Identifier` 备用数据流）；③ 没有"受信任的发布者"签名 |
| 「Windows 已保护你的电脑 —— 阻止了无法识别的应用启动」（不运行 / 更多信息） | Microsoft Defender SmartScreen | 带 MotW 的**低信誉**文件（新编译的未签名文件信誉≈0）|

还要注意本机的第三种可能：**智能应用控制（Smart App Control）**，它和签名是两件事，见第 6 节。

### 1.1 本机实测：签名与标记的现状

* 本机**没有任何代码签名证书**：`Cert:\CurrentUser\My`、`Cert:\LocalMachine\My` 都是空的，
  `Cert:\CurrentUser\TrustedPublisher` 也是空的；
  `Get-AuthenticodeSignature ..\_rust\target-cap\debug\CapLite.exe` → **`NotSigned`**。
* **每次重新编译都是一个全新的未签名文件**。实测（强制重新编译）：

  ```powershell
  Remove-Item -LiteralPath ..\_rust\target-cap\debug\CapLite.exe,..\_rust\target-cap\debug\deps\CapLite.exe -Force
  cargo build --bin CapLite          # Finished `dev` profile ... in 5.05s
  Get-AuthenticodeSignature -LiteralPath ..\_rust\target-cap\debug\CapLite.exe   # -> NotSigned
  ```

  所以"手动点一次运行"不会被记住：exe 换了，一切归零。

### 1.2 本机实测：`Zone.Identifier` 到底在哪里

```powershell
Get-Item -LiteralPath ..\_rust\target-cap\debug\CapLite.exe -Stream *        # 只有 :$DATA
Get-Content -LiteralPath ..\_rust\target-cap\debug\CapLite.exe -Stream Zone.Identifier   # 不存在
```

* `CapLite.exe`（以及 `audio_probe.exe`/`record_smoke.exe`/`build-script-build.exe`/`deps\*.exe`）
  **没有** `Zone.Identifier`；
* 把工作区 803 个 exe 全扫一遍，只有 **9 个**带标记，全部是**下载来的 ffmpeg 二进制**，
  内容是 `ZoneId=3` + `ReferrerUrl=https://github.com/BtbN/FFmpeg-Builds/releases` ——
  即"从 GitHub Release 下载解压"留下的，来源明确；
* 连**目录**级标记也查了（MotW 也可能挂在目录上）：
  `_rust\target-cap\debug`、`_rust\target-cap`、`_rust`、工作区根、`CapLite`、`CapLite\scripts`
  以及下载来的 `ffmpeg-...\bin`，`Get-Item <目录> -Stream *` **全部没有** `Zone.Identifier`。
* 也就是说：**`cargo build` 与本次交付的脚本都不会给产物打 MotW**（重新编译后的 exe 现查现证：没有）。
  如果在你机器上 `CapLite.exe` 曾经带标记，那只能是它被某个下载/复制/同步/中转工具经手过 ——
  本次实测**无法进一步定位到具体是哪一步**，但脚本每次签名前都会清掉它，所以不影响使用。

## 2. 方案做了什么

### 2.1 `scripts/dev-sign.ps1`：证书 → 信任 → 清标记 → 签名 → 回读

幂等，可反复运行：

1. **证书**：`Cert:\CurrentUser\My` 里已有 `CN=CapLite dev signing`
   （有私钥 + `Code Signing` 用途 OID `1.3.6.1.5.5.7.3.3` + 未过期）就**复用**；否则新建：

   ```powershell
   New-SelfSignedCertificate -Type CodeSigningCert -KeyUsage DigitalSignature `
     -Subject "CN=CapLite dev signing" -CertStoreLocation Cert:\CurrentUser\My `
     -NotAfter (Get-Date).AddYears(5)
   ```

   自签名 = 签发者就是自己，不需要 CA、不联网、不花钱。
2. **信任**：`Export-Certificate` 只导出**公钥证书**（`.cer`，不含私钥）→
   装进 `Cert:\CurrentUser\TrustedPublisher`（受信任的发布者）；装完**立刻回读实测**并打印
   `TRUST-PROBE` 结论；若此时还不是 `Valid`，再装进 `Cert:\CurrentUser\Root`（受信任的根）并复测。
   * 私钥**始终只留在** `Cert:\CurrentUser\My` 的密钥库里，不会进受信任存储（可用 `-NoTrustedRoot` 禁止碰根）。
   * 为什么可能需要根：`Get-AuthenticodeSignature` 走标准 X509 证书链校验，链必须终止在一张
     **受信任的根证书**上才可能 `Valid`。本机实测的中间状态是
     `Status=UnknownError` + `StatusMessage=已处理证书链，但是在不受信任提供程序信任的根证书中终止。`
     （"受信任的发布者"存储的作用是让 Windows 把该证书的签名者视为可信发布者，
     见微软文档 [Trusted Publishers Certificate Store](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/trusted-publishers-certificate-store)。）
3. **清 Mark-of-the-Web**：逐个目标 exe 检查 `Zone.Identifier`，存在就 `Unblock-File`，
   仍存在则 `Remove-Item -Stream Zone.Identifier`，最后**复查确认已消失**；
   同时检查各级**父目录**上的同名流并同样清除（目录带标记时 Windows 也会对里面的文件做附件检查）。
4. **签名**：`Set-AuthenticodeSignature -HashAlgorithm SHA256`（覆盖旧签名，重复跑安全）。
   若同名进程在运行会提前警告，失败信息可读（"文件被占用"）。
5. **回读**：逐个打印 `Status` / `StatusMessage` / 签名者主题与指纹 / MotW 是否残留。
   全部 `Valid` 且 MotW 全清才 `exit 0`。
   退出码：`0` 通过、`1` 有文件不是 Valid、`2` 没找到可签名文件、`3` 证书环节失败、`4` 信任存储装不进去。

### 2.2 `scripts/run-dev.ps1`：构建 → 签名 → 启动（**启动不走 Shell**）

`cargo build --bin CapLite` → `dev-sign.ps1` → 启动 `CapLite.exe`。
环境引导与 `regression.ps1` 相同（PATH 里没有 cargo 时按 UTF-8 读取并执行
`<工作区>\_rust\env.ps1`，`CARGO_TARGET_DIR` 固定为 `<工作区>\_rust\target-cap`），
所以在**没有任何 cargo 环境变量的普通 PowerShell** 里可以直接跑通。

**启动默认用 `ProcessStartInfo(UseShellExecute = $false)`，即直接 CreateProcess，不经过 Shell** ——
「无法验证发布者」这个**模态**框长在 ShellExecuteEx 那条路上，不走它就永远不会出现，
也不会卡住自动化。加 `-UseShellExecute` 才回到 `Start-Process` 的老路（仅供对比排障）。

### 2.3 实测：ShellExecute vs 直接 CreateProcess

同一批 exe（放在 `_rust\target-cap\debug\` 里，依赖 DLL 齐全），
探针脚本分别用 `Start-Process`（默认 `UseShellExecute = $true`）和
`[System.Diagnostics.Process]::Start($psi)`（`UseShellExecute = $false`，等价于 `&` 调用操作符）
启动，测"调用返回耗时"和"3 秒后进程是否还在"：

| 被启动的文件状态 | 启动方式 | 调用返回耗时 | 3 秒后 |
| --- | --- | --- | --- |
| 未签名 + **带 MotW** | `Start-Process`（ShellExecute） | **9415 ms** | 进程在跑，主窗口 `CapLite`，Responding=True |
| 未签名 + **带 MotW** | `ProcessStartInfo(UseShellExecute=$false)` | **114 ms** | 进程在跑，同上 |
| 未签名 + 无 MotW | `Start-Process` | 807 ms | 进程在跑 |
| 签名**不可验证** + 带 MotW | `Start-Process` | 3064 ms | 进程在跑 |
| 签名**不可验证** + 无 MotW | `Start-Process` | 768 ms | 进程在跑 |
| 真实的未签名 `CapLite.exe`（无 MotW） | `Start-Process` | 2637 ms | 进程在跑 |
| 真实的未签名 `CapLite.exe`（无 MotW） | `ProcessStartInfo(UseShellExecute=$false)` | **19 ms** | 进程在跑 |

怎么读这张表：

* 走 Shell 的启动**明显更慢**，而且**带 MotW 时慢得离谱**（0.8 s → 9.4 s）：
  那几秒就是 Shell 在做区域/附件/签名（SmartScreen 云查询）评估 ——
  「无法验证发布者。你确定要运行某软件吗？」这个**等待用户点击**的模态框，
  就出现在这段评估里（在交互式桌面会话里它会一直等，直到点"运行"）。
* 直接 CreateProcess（`UseShellExecute = $false`，以及 PowerShell 里的 `& .\CapLite.exe`、
  `cmd /c CapLite.exe`）**完全不进入这段评估**：19~114 ms 就起来了，进程、窗口、stderr 输出都正常
  （`[cap] 配置文件目录/输出目录: ...\debug\cap-recordings`）。
* 坦白说明：我这个会话**看不到 UI，也没有复现出弹框**（上面所有 `Start-Process` 都没有阻塞，
  连"未签名 + 带 MotW"那一条也只是慢、没有卡住）。所以"不弹框"的结论来自
  **机制（不经过 Shell 的检查路径）+ 耗时证据（不进入那 9 秒的评估）**，
  而不是"我肉眼看到框消失了"。

## 3. 怎么用

```powershell
# 只签名（编译完就跑这个；默认签 _rust\target-cap\debug\*.exe 和 release\*.exe）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1

# 一键：构建 → 清标记 + 签名 → 启动（启动不走 Shell，不会弹"无法验证发布者"）
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-dev.ps1
```

其它常用写法：

```powershell
# 只签某些文件（-File 调用时请用**逗号写成一个字符串**，不要写数组语法）
... -File scripts\dev-sign.ps1 -Path "..\_rust\target-cap\debug\CapLite.exe,..\_rust\target-cap\debug\clip_smoke.exe"

# 已构建过：跳过构建，只重新签名 + 启动
... -File scripts\run-dev.ps1 -NoBuild

# release 产物 / 复现老启动方式（对比用）/ 受限沙箱里验证签名流程
... -File scripts\run-dev.ps1 -Release
... -File scripts\run-dev.ps1 -NoBuild -UseShellExecute
... -File scripts\run-dev.ps1 -NoBuild -PortableKeyFallback
```

**重新编译之后要做什么？** 只做一件事：**再跑一次 `dev-sign.ps1`**（或直接用 `run-dev.ps1`）。
证书不用重建（脚本复用），exe 会被重新签名。日常开发只记 `run-dev.ps1` 一条命令即可。

### 自检（在你的普通 PowerShell 里跑，期望 `Valid`）

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\dev-sign.ps1
# 期望结尾：PASS：全部可执行文件已签名且 Status=Valid，且没有 Zone.Identifier 标记。
# 并留意 TRUST-PROBE 那一行，它会告诉你"只装 TrustedPublisher 是否已经足够"。

Get-AuthenticodeSignature -LiteralPath ..\_rust\target-cap\debug\CapLite.exe |
  Format-List Status, StatusMessage, SignerCertificate
Get-Item -LiteralPath ..\_rust\target-cap\debug\CapLite.exe -Stream *     # 只应看到 :$DATA
```

> **本次交付的验证边界（如实说明）**：我执行脚本的会话是**受限写入**的沙箱，
> 证书存储写入被系统拒绝：
> `CertEnroll::CX509Enrollment::_CreateRequest: 拒绝访问。0x80090010 (NTE_PERM)`、
> `certutil: -addstore 失败: 0x80070005 (ERROR_ACCESS_DENIED)`。
> 因此我能实测到的是：**签名确实写进了每个 exe**（回读签名者 = `CN=CapLite dev signing`、
> `SignatureType=Authenticode`）、**MotW 检测与清除确实有效**、**启动方式差异确实存在**；
> 但**"Status=Valid"这一步必须在能写当前用户证书存储的普通 PowerShell 里跑才能得到**。
> 脚本已经把这一步做成自动判定 + 自动补装 Root + 明确 PASS/FAIL，
> 请以那一次运行的输出为准，不要以我这里的 `UnknownError` 为准（那是沙箱权限造成的，
> `StatusMessage` 明确写着"证书链在不受信任的根证书中终止"，即**信任锚缺失，不是签名没写上**）。
> 只在小范围排障时可以用 `-PortableKeyFallback` 在无权限环境里验证签名流程，
> 它的产物状态**不会是 Valid**，脚本会大声警告这一点。

### 在受限环境里运行会发生什么（脚本的报错设计）

证书存储不可写时，脚本打印可读原因与解决办法（"请在普通 PowerShell 窗口运行"）后
`exit 3`，**不会**静默失败、也不会留下"看着像签好了"的假象。
`-PortableKeyFallback` 会退化为"工作区 PFX 私钥 + 内存密钥（EphemeralKeySet）"签名，
签名能写进文件但信任装不进去，脚本会警告。

## 4. 清 MotW 的手工办法与它和签名的关系

```powershell
Get-Item .\_rust\target-cap\debug\CapLite.exe -Stream *      # 只看到 :$DATA 就是没有标记
Unblock-File -LiteralPath .\_rust\target-cap\debug\CapLite.exe                 # 首选（官方 API）
# 兜底（Unblock-File 没效果时用；对**目录**上的流也只有这个办法管用 ——
# `Remove-Item -Stream` 在目录上会要求交互确认，非交互会话里直接失败）：
[System.IO.File]::Delete('C:\...\_rust\target-cap\debug\CapLite.exe:Zone.Identifier')
```

目录上的标记（`Get-Content <目录> -Stream Zone.Identifier` 也能读到，实测有效）同样要清：
`Unblock-File` 对目录**实测无效**（报成功但不删流），必须用上面的 .NET 删流写法。
`dev-sign.ps1` 已经把"文件 + 各级父目录"两条都做了自动检查与清除（实测：目录流和文件流都能被清掉并复查为空）。

| | 有 MotW | 无 MotW |
| --- | --- | --- |
| 未签名 | Shell 启动会弹「无法验证发布者」；SmartScreen 可能拦 | 直接运行（但走 Shell 的签名评估仍在） |
| 有**本机受信任**的自签名 | 发布者已可信 → 该确认框不再出现；SmartScreen 仍可能因**信誉不足**提示 | 正常 |
| 有受信任 CA（OV/EV）的签名 | 不弹；EV 自带信誉，SmartScreen 一般不拦 | 正常 |

**标记与签名有效性是两件事**：MotW 决定"要不要做区域检查"，签名决定"发布者是否可信"。
两个都处理掉，才同时覆盖两条弹框路径。

## 5. 局限（别误解）

* **自签名只在本机、本用户受信任**。别人拿到这个 `CapLite.exe` 仍是"未知发布者"，
  SmartScreen 仍会拦；换一台机器就是未签名效果。
* **发布给他人必须用受信任 CA 的代码签名证书**（OV/EV），并用时间戳
  （`Set-AuthenticodeSignature -TimestampServer ...`）。本项目是离线开发环境，脚本**没有**加时间戳；
  自签名证书有效期 5 年，够开发用（证书过期后旧签名会失效，重跑脚本即可）。
* **SmartScreen 看的是"信誉"，不是"签名是否有效"**。即使 `Valid`，带 MotW 的低信誉文件仍可能提示
  （"更多信息 → 仍要运行"可放行）。签名能消掉的是"无法验证发布者"这一类框。
* **智能应用控制（Smart App Control）与签名是两件事**：微软文档写明，强制模式下
  "应用无法运行，除非 Microsoft 的应用智能服务识别它们，或者使用**受信任根计划内的 CA**签发的证书签名"。
  也就是**本地自签名证书不可能让 SAC 放行**。本机当前是**评估模式**
  （`HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy\VerifiedAndReputablePolicyState = 2`），
  见 [智能应用控制概述](https://learn.microsoft.com/zh-cn/windows/apps/develop/smart-app-control/overview)。
  本脚本**不碰** SAC / SmartScreen / Defender —— 关掉它们都是削弱安全，不在方案范围内。
* 脚本要求能写**当前用户**证书存储；受限沙箱/无权限会话会失败（可读报错 + `exit 3`）。
* 签名时 exe 不能被占用（`CapLite.exe` 正在运行就先 `Stop-Process -Name cap`）。
* 两个 `.ps1` 存成 **UTF-8 with BOM**（与 `scripts/regression.ps1` 一致），
  这样 Windows PowerShell 5.1 也能正确读中文；`-File` 调用时多值参数请用逗号写成一个字符串
  （脚本已经 `PositionalBinding = $false`，参数写错会直接报错而不会静默错绑）。

## 6. 如果签名+清标记后**仍然**会弹，怎么办

按可能性从高到低：

1. **确认此刻真的没有标记**：
   `Get-Item <exe> -Stream *`（只应有 `:$DATA`）+ 检查各级父目录同名流。
   `dev-sign.ps1` 每次都会自动做这两件事并打印结论。
2. **启动方式换成不走 Shell 的**（立刻见效、不依赖任何信任设置）：
   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\run-dev.ps1 -NoBuild   # 默认就不走 Shell
   cmd /c "C:\...\_rust\target-cap\debug\CapLite.exe"                                     # 也是直接 CreateProcess
   & "C:\...\_rust\target-cap\debug\CapLite.exe"                                          # PowerShell 控制台里
   ```
   而**资源管理器双击**、快捷方式、`Start-Process`（默认）都会走 ShellExecuteEx，
   也就是都会经过那段区域/签名评估。
3. **确认信任装上了**：`Get-AuthenticodeSignature` 应为 `Valid`；看脚本的 `TRUST-PROBE` 结论，
   若它说"只装 TrustedPublisher 不够"，说明脚本已把证书同时装进 `Cert:\CurrentUser\Root`。
4. **机器策略要求的"始终警告"**：如果附件管理器/Internet 选项里把
   "启动应用程序和不安全文件"设成了"提示"，那它是**系统策略**，不是文件能改的。
   注意：**清掉 MotW 之后这类策略对本地文件不再生效**（没有区域信息就没有附件检查）。
   把目录加进"本地 Intranet/受信任站点"（Internet 选项 → 安全 → 本地 Intranet → 站点 → 高级，
   加 `file:///C:/...` 形式）也能绕过，但那是在**放宽系统信任**，属于你自己的安全决定，
   本脚本不会替你做。
5. **智能应用控制在强制模式**（设置 → Windows 安全 → 应用和浏览器控制）：
   自签名无效，只能改用 CA 证书，或在系统设置里关掉它（不可逆，且属于削弱安全，脚本不做）。

## 7. 如何撤销

```powershell
$subject = 'CN=CapLite dev signing'

# 0) 先看看都有哪些（记下 Thumbprint）
Get-ChildItem Cert:\CurrentUser\My               | Where-Object Subject -eq $subject | Format-List Subject,Thumbprint
Get-ChildItem Cert:\CurrentUser\TrustedPublisher | Where-Object Subject -eq $subject | Format-List Subject,Thumbprint
Get-ChildItem Cert:\CurrentUser\Root             | Where-Object Subject -eq $subject | Format-List Subject,Thumbprint

# 1) 删证书（"个人"存储里的那张，私钥一起删）
Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert |
  Where-Object Subject -eq $subject | Remove-Item -Force

# 2) 删"受信任的发布者"里的条目
Get-ChildItem Cert:\CurrentUser\TrustedPublisher |
  Where-Object Subject -eq $subject | Remove-Item -Force
# 等价（用指纹，不弹 UI）：
# certutil -user -delstore TrustedPublisher <Thumbprint>

# 3) 若脚本装过"受信任的根"，也删掉
Get-ChildItem Cert:\CurrentUser\Root |
  Where-Object Subject -eq $subject | Remove-Item -Force
# 等价（用指纹，不弹 UI）：
# certutil -user -delstore Root <Thumbprint>

# 4) 删掉脚本留下的公钥证书 / 便携私钥（如果有）
Remove-Item -LiteralPath <工作区>\_rust\target-cap\dev-signing -Recurse -Force
```

撤销后：**已写进 exe 的签名还在文件里**，但没有受信任证书，回读会变成 `UnknownError`
（"不受信任的根"），等价于未签名；对"没有 MotW 的本地文件"没有实际影响。
要彻底干净，重新构建一次即可（`cargo build --bin CapLite`，或 `scripts\run-dev.ps1`）。

## 8. 常见问题

| 现象 | 原因 / 处理 |
| --- | --- |
| `NTE_PERM` / `0x80090010` | 当前会话不能写用户证书/密钥库（沙箱或权限受限）。**改到普通 PowerShell 窗口**重跑 |
| `certutil: -addstore 失败: 0x80070005` | 同上，写证书存储被拒绝 |
| 回读是 `UnknownError`，`StatusMessage` 提到"不受信任的根" | 证书没进受信任存储（或只进了发布者）。重跑脚本让 `TRUST-PROBE` 自动补装 Root |
| `Import-Certificate` 装 Root 报"此操作中不允许使用 UI" | 装根证书需交互确认；脚本优先用 `certutil -user -f -addstore Root`（不弹 UI）。请在普通桌面会话运行 |
| 签名失败、提示文件被占用 | 关掉正在运行的 `CapLite.exe`（`Stop-Process -Name cap`）后重跑 |
| 签名后仍看到 SmartScreen | 见第 5、6 节：SmartScreen 看信誉；先确认 `-Stream *` 里没有 `Zone.Identifier` |
| 重新编译后又弹框 | 正常：新 exe 未签名。重跑 `scripts\run-dev.ps1`（或 `dev-sign.ps1`） |
| `-File` 传多个 `-Path` 报错/错绑 | 用逗号写成一个字符串：`-Path "a.exe,b.exe"` |
