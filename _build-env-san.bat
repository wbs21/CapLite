@echo off
rem ---------------------------------------------------------------------------
rem 沙箱安全版构建环境。与 _build-env.bat 的唯一区别：
rem   **不调用 vcvars64.bat** —— 它内部会启动 reg.exe 探测 SDK 注册表项，
rem   而 reg.exe 在本机 Security Center 的 Program Blacklist 里，沙箱内必被拦
rem   （且该黑名单"无法从当前命令批准或绕过"，见沙箱拦截提示原文）。
rem 这里改为**静态路径**，全部来自本机实测安装位置：
rem   VS 18 Community / MSVC 14.51.36231 / Windows SDK 10.0.28000.0
rem 正常（非沙箱）环境请仍用 _build-env.bat（会随 vcvars 自动跟随升级）。
rem 维护提醒：VS/SDK 升级后需要同步改下面的两行版本号。
rem ---------------------------------------------------------------------------
set VSLANG=1033
rem 用脚本自身目录，而不是写死某一份工作副本：workspace / 原目录都能直接构建。
cd /d "%~dp0"

rem ---- MSVC 工具链（静态，代替 vcvars64.bat 干的活） ----
set "VCTOOLS=C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.51.36231"
set "PATH=%PATH%;%VCTOOLS%\bin\Hostx64\x64"

rem ---- Windows SDK 10.0.28000.0（与 _build-env.bat 相同的手工补齐） ----
set "SDKINC=C:/Program Files (x86)/Windows Kits/10/Include/10.0.28000.0"
set "SDKLIB=C:/Program Files (x86)/Windows Kits/10/Lib/10.0.28000.0"
set "SDKBIN=C:/Program Files (x86)/Windows Kits/10/bin/10.0.28000.0/x64"
set "PATH=%PATH%;%SDKBIN%"

rem ---- INCLUDE / LIB（vcvars 原本逐项追加的顺序照抄） ----
set "INCLUDE=%VCTOOLS%\include;%SDKINC%\ucrt;%SDKINC%\shared;%SDKINC%\um;%SDKINC%\winrt"
set "LIB=%VCTOOLS%\lib\x64;%SDKLIB%\ucrt\x64;%SDKLIB%\um\x64"

rem ---- 让 cc crate 直接 PATH 找 cl.exe，不去查注册表 ----
set "CC=cl"
set "CXX=cl"

rem ---- 构建卫生：本机测试会跑录制 / FFmpeg / openh264 ----
set "FFMPEG_DIR=%~dp0ffmpeg-n9.0-latest-win64-lgpl-shared-9.0"
set "LIBCLANG_PATH=%~dp0_llvm\bin"
set CARGO_INCREMENTAL=0

echo [san-env] VCTOOLS=%VCTOOLS%
echo [san-env] CARGO_INCREMENTAL=%CARGO_INCREMENTAL%
echo [san-env] FFMPEG_DIR=%FFMPEG_DIR%
where cl.exe
cargo %*
